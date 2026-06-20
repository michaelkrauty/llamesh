//! Pure decision logic for the wedged-instance detector.
//!
//! A local `llama-server` instance that is holding a request slot
//! (`in_flight_requests > 0`) but is producing nothing — flat at ~0% CPU and
//! ~0% GPU — cannot be generating the response the client is waiting on. Such an
//! instance pins its slot forever (the request never completes), which stalls a
//! graceful drain and blocks idle eviction (the same failure
//! `upstream_read_timeout_ms` bounds per-request). This module decides, from a
//! single activity sample, whether an instance looks wedged *this tick*; the
//! caller layers a sustained-time window on top before acting, so a transient
//! lull is never mistaken for a wedge.
//!
//! All logic here is pure and I/O-free so it can be unit-tested exhaustively;
//! the procfs/NVML sampling lives in the caller.

/// CPU usage, in busy-cores over the sample interval, at or below which the
/// process counts as "not doing CPU work". A real prefill or token loop drives
/// at least one core well above this; a process blocked on a socket sits at ~0.
pub const CPU_EPS_CORES: f64 = 0.02;

/// Per-process GPU SM utilization (percent) at or below which the GPU counts as
/// idle for the process. A live decode reports a non-zero SM utilization.
pub const GPU_EPS_UTIL: u32 = 1;

/// One activity sample for a single instance, plus the thresholds to judge it.
#[derive(Debug, Clone, Copy)]
pub struct WedgeInputs {
    /// Requests currently occupying a slot on this instance.
    pub in_flight: usize,
    /// CPU busy-cores measured over the last interval, or `None` when no prior
    /// sample exists yet (the first observation of a pid). Without a delta we
    /// cannot judge activity, so the instance is treated as not wedged.
    pub cpu_busy_cores: Option<f64>,
    /// Per-process GPU SM utilization (percent). `None` means "no reading":
    /// NVML reports utilization only for processes that were *busy* in the
    /// sample window, so an idle process is simply absent — never `Some(0)`.
    /// `None` is therefore ambiguous and is resolved using `gpu_present` /
    /// `gpu_util_trusted` below.
    pub gpu_sm_util: Option<u32>,
    /// Whether this node has at least one visible NVML (NVIDIA) GPU. When
    /// false the node may still have a non-NVIDIA GPU (Metal/ROCm/Vulkan) whose
    /// activity we cannot see, so idle CPU alone is not proof of a wedge unless
    /// `cpu_only` is asserted.
    pub gpu_present: bool,
    /// Operator assertion that this node runs inference on CPU only (no GPU of
    /// any vendor). Only then does idle CPU while holding a slot, with no NVML
    /// GPU present, count as a wedge. Defaults off so a node with a non-NVIDIA
    /// GPU is never wrongly flagged.
    pub cpu_only: bool,
    /// Whether per-process GPU utilization has been observed working on this
    /// node at least once (some process reported `sm_util > GPU_EPS_UTIL`).
    /// Until proven, a `None` reading on a GPU node is untrustworthy — the
    /// driver may not surface per-process utilization on this hardware — so we
    /// never flag on CPU alone, guarding against a node where the GPU signal is
    /// entirely unavailable.
    pub gpu_util_trusted: bool,
    /// CPU idle threshold (busy-cores).
    pub cpu_eps_cores: f64,
    /// GPU idle threshold (percent).
    pub gpu_eps_util: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WedgeVerdict {
    /// The instance shows activity, or we cannot safely judge it this tick.
    NotWedged,
    /// The instance holds a slot but shows no CPU and no GPU activity this tick.
    WedgedThisTick,
}

/// Decide whether an instance looks wedged for a single activity sample.
///
/// Conservative by construction: any positive evidence of work, any missing
/// signal we cannot trust, or simply not holding a slot, all yield `NotWedged`.
pub fn evaluate_wedge(i: &WedgeInputs) -> WedgeVerdict {
    // An instance not serving a request is expected to sit idle; never flag it.
    if i.in_flight == 0 {
        return WedgeVerdict::NotWedged;
    }

    // Without a CPU delta (first observation) we cannot judge activity yet.
    let Some(cpu) = i.cpu_busy_cores else {
        return WedgeVerdict::NotWedged;
    };

    // Any meaningful CPU work — prefill, tokenization, a CPU-side decode loop —
    // means the instance is alive. Only an idle CPU is a wedge candidate.
    if cpu > i.cpu_eps_cores {
        return WedgeVerdict::NotWedged;
    }

    match i.gpu_sm_util {
        // A real per-process GPU reading: the GPU must also be idle to be wedged.
        // A live GPU-bound decode reports `sm_util > 0` even while the CPU is
        // idle, so this is the guard that protects normal token generation.
        Some(util) => {
            if util <= i.gpu_eps_util {
                WedgeVerdict::WedgedThisTick
            } else {
                WedgeVerdict::NotWedged
            }
        }
        // No per-process GPU reading for this pid.
        None => {
            if i.gpu_present {
                if i.gpu_util_trusted {
                    // NVIDIA GPU node where per-process util is known to work and
                    // this tick's sweep was usable: absence means genuinely zero
                    // GPU activity → wedged.
                    WedgeVerdict::WedgedThisTick
                } else {
                    // Per-process util has not proven itself (or the sweep was
                    // degraded): an absent reading is not trustworthy, so do not
                    // flag on CPU alone.
                    WedgeVerdict::NotWedged
                }
            } else if i.cpu_only {
                // Operator asserts no GPU does inference here, so idle CPU while
                // holding a slot is itself the wedge signal.
                WedgeVerdict::WedgedThisTick
            } else {
                // No NVML GPU and no CPU-only assertion: a non-NVIDIA GPU could
                // be doing the work invisibly, so idle CPU is not proof of a
                // wedge.
                WedgeVerdict::NotWedged
            }
        }
    }
}

/// CPU busy-cores between two cumulative `(utime + stime)` tick readings.
///
/// `1.0` is one fully-busy core; a multithreaded server can exceed `1.0`.
/// Returns `0.0` for a non-positive interval or a tick counter that did not
/// advance (e.g. a reset or pid reuse), both of which read as "idle" and are
/// caught by the sustained-window confirmation in the caller.
pub fn cpu_busy_cores(
    busy_now: u64,
    busy_prev: u64,
    ticks_per_sec: u64,
    interval_secs: f64,
) -> f64 {
    if ticks_per_sec == 0 || interval_secs <= 0.0 {
        return 0.0;
    }
    let delta = busy_now.saturating_sub(busy_prev) as f64;
    delta / (ticks_per_sec as f64 * interval_secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[allow(clippy::too_many_arguments)]
    fn inputs(
        in_flight: usize,
        cpu: Option<f64>,
        gpu: Option<u32>,
        gpu_present: bool,
        gpu_util_trusted: bool,
        cpu_only: bool,
    ) -> WedgeInputs {
        WedgeInputs {
            in_flight,
            cpu_busy_cores: cpu,
            gpu_sm_util: gpu,
            gpu_present,
            cpu_only,
            gpu_util_trusted,
            cpu_eps_cores: CPU_EPS_CORES,
            gpu_eps_util: GPU_EPS_UTIL,
        }
    }

    #[test]
    fn no_inflight_is_never_wedged() {
        // Even flat 0/0 on a trusted GPU node: an idle instance holding no slot
        // is expected to be quiet and must never be flagged.
        assert_eq!(
            evaluate_wedge(&inputs(0, Some(0.0), Some(0), true, true, false)),
            WedgeVerdict::NotWedged
        );
        assert_eq!(
            evaluate_wedge(&inputs(0, None, None, false, false, false)),
            WedgeVerdict::NotWedged
        );
    }

    #[test]
    fn first_tick_without_cpu_delta_is_not_wedged() {
        assert_eq!(
            evaluate_wedge(&inputs(1, None, Some(0), true, true, false)),
            WedgeVerdict::NotWedged
        );
        assert_eq!(
            evaluate_wedge(&inputs(1, None, None, false, false, false)),
            WedgeVerdict::NotWedged
        );
    }

    #[test]
    fn busy_cpu_is_not_wedged() {
        // CPU above epsilon → working, regardless of the GPU signal.
        assert_eq!(
            evaluate_wedge(&inputs(1, Some(2.5), Some(0), true, true, false)),
            WedgeVerdict::NotWedged
        );
        assert_eq!(
            evaluate_wedge(&inputs(1, Some(0.5), None, false, false, false)),
            WedgeVerdict::NotWedged
        );
    }

    #[test]
    fn gpu_busy_with_idle_cpu_is_not_wedged() {
        // The critical false-positive guard: a GPU-bound decode (CPU idle, GPU hot).
        assert_eq!(
            evaluate_wedge(&inputs(1, Some(0.0), Some(45), true, true, false)),
            WedgeVerdict::NotWedged
        );
    }

    #[test]
    fn flat_cpu_and_gpu_on_gpu_node_is_wedged() {
        // The textbook fingerprint: holds a slot, CPU idle, a real GPU reading of 0.
        assert_eq!(
            evaluate_wedge(&inputs(1, Some(0.0), Some(0), true, true, false)),
            WedgeVerdict::WedgedThisTick
        );
    }

    #[test]
    fn gpu_eps_boundary() {
        // At/below the GPU epsilon counts as idle; just above counts as busy.
        assert_eq!(
            evaluate_wedge(&inputs(1, Some(0.0), Some(GPU_EPS_UTIL), true, true, false)),
            WedgeVerdict::WedgedThisTick
        );
        assert_eq!(
            evaluate_wedge(&inputs(
                1,
                Some(0.0),
                Some(GPU_EPS_UTIL + 1),
                true,
                true,
                false
            )),
            WedgeVerdict::NotWedged
        );
    }

    #[test]
    fn cpu_eps_boundary() {
        // Exactly at the CPU epsilon is still "idle" (<=); just above is "busy".
        assert_eq!(
            evaluate_wedge(&inputs(1, Some(CPU_EPS_CORES), Some(0), true, true, false)),
            WedgeVerdict::WedgedThisTick
        );
        assert_eq!(
            evaluate_wedge(&inputs(
                1,
                Some(CPU_EPS_CORES + 1e-9),
                Some(0),
                true,
                true,
                false
            )),
            WedgeVerdict::NotWedged
        );
    }

    #[test]
    fn cpu_only_node_flat_cpu_is_wedged() {
        // No NVML GPU and the operator asserts cpu_only: idle CPU while holding
        // a slot is the wedge signal.
        assert_eq!(
            evaluate_wedge(&inputs(1, Some(0.0), None, false, false, true)),
            WedgeVerdict::WedgedThisTick
        );
    }

    #[test]
    fn no_nvml_without_cpu_only_assertion_is_not_wedged() {
        // No NVML GPU and cpu_only NOT asserted: a non-NVIDIA GPU (Metal/ROCm/
        // Vulkan) could be doing the work invisibly, so idle CPU is not proof.
        assert_eq!(
            evaluate_wedge(&inputs(1, Some(0.0), None, false, false, false)),
            WedgeVerdict::NotWedged
        );
    }

    #[test]
    fn gpu_node_with_untrusted_util_does_not_flag_on_cpu_alone() {
        // GPU present but per-process util never proven working: None is ambiguous.
        assert_eq!(
            evaluate_wedge(&inputs(1, Some(0.0), None, true, false, false)),
            WedgeVerdict::NotWedged
        );
    }

    #[test]
    fn gpu_node_with_trusted_util_flags_absent_pid() {
        // GPU present, util proven working, pid absent from the sample → idle GPU.
        assert_eq!(
            evaluate_wedge(&inputs(1, Some(0.0), None, true, true, false)),
            WedgeVerdict::WedgedThisTick
        );
    }

    #[test]
    fn cpu_busy_cores_math() {
        // 100 ticks over 1s at 100 Hz = 1.0 core.
        assert!((cpu_busy_cores(300, 200, 100, 1.0) - 1.0).abs() < 1e-9);
        // No advance → 0.
        assert_eq!(cpu_busy_cores(200, 200, 100, 1.0), 0.0);
        // Counter went backwards (reset / pid reuse) → 0, not negative.
        assert_eq!(cpu_busy_cores(100, 200, 100, 1.0), 0.0);
        // 50 ticks over 5s at 100 Hz = 0.1 core.
        assert!((cpu_busy_cores(50, 0, 100, 5.0) - 0.1).abs() < 1e-9);
    }

    #[test]
    fn cpu_busy_cores_guards_degenerate_interval() {
        assert_eq!(cpu_busy_cores(300, 200, 0, 1.0), 0.0); // zero ticks_per_sec
        assert_eq!(cpu_busy_cores(300, 200, 100, 0.0), 0.0); // zero interval
        assert_eq!(cpu_busy_cores(300, 200, 100, -1.0), 0.0); // negative interval
    }
}
