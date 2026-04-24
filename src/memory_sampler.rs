use nvml_wrapper::enums::device::UsedGpuMemory;
use nvml_wrapper::Nvml;
use procfs::process::Process;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use tracing::{debug, warn};

const BYTES_PER_MIB: u64 = 1024 * 1024;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuDeviceMemory {
    pub index: u32,
    pub used_mb: u64,
    pub free_mb: u64,
    pub total_mb: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GpuMemorySnapshot {
    pub used_mb: u64,
    pub free_mb: u64,
    pub total_mb: u64,
    pub devices: Vec<GpuDeviceMemory>,
}

/// Samples actual memory usage (VRAM and system memory) from running processes.
pub struct MemorySampler {
    nvml: Option<Nvml>,
    #[cfg(test)]
    device_vram_override: std::sync::Mutex<Option<GpuMemorySnapshot>>,
}

fn bytes_to_mib_floor(bytes: u64) -> u64 {
    bytes / BYTES_PER_MIB
}

fn bytes_to_mib_ceil(bytes: u64) -> u64 {
    bytes.div_ceil(BYTES_PER_MIB)
}

impl MemorySampler {
    pub fn new() -> Self {
        let nvml = match Nvml::init() {
            Ok(nvml) => {
                debug!("NVML initialized successfully");
                Some(nvml)
            }
            Err(e) => {
                // Provide more helpful diagnostics based on whether GPU hardware exists
                let nvidia_device_exists = std::path::Path::new("/dev/nvidia0").exists();
                if nvidia_device_exists {
                    warn!(
                        "NVML init failed but NVIDIA device detected: {}. \
                         VRAM sampling disabled - check NVIDIA driver installation.",
                        e
                    );
                } else {
                    debug!(
                        "NVML init failed (no NVIDIA GPU detected): {}; VRAM sampling disabled",
                        e
                    );
                }
                None
            }
        };
        Self {
            nvml,
            #[cfg(test)]
            device_vram_override: std::sync::Mutex::new(None),
        }
    }

    #[cfg(test)]
    pub fn set_device_vram_override(&self, snapshot: Option<GpuMemorySnapshot>) {
        *self.device_vram_override.lock().unwrap() = snapshot;
    }

    /// Sample system memory (RSS) for a process in MiB.
    pub fn sample_sysmem(&self, pid: u32) -> Option<u64> {
        let proc = Process::new(pid as i32).ok()?;
        let status = proc.status().ok()?;
        // VmRSS is in KiB, convert to MiB
        status.vmrss.map(|kb| kb / 1024)
    }

    /// Sample VRAM usage for a process in MiB.
    /// Sums across all GPUs where the process appears.
    pub fn sample_vram(&self, pid: u32) -> Option<u64> {
        let nvml = self.nvml.as_ref()?;
        let device_count = nvml.device_count().ok()?;

        let mut total_vram_bytes = 0u64;
        for i in 0..device_count {
            if let Ok(device) = nvml.device_by_index(i) {
                let mut by_pid: HashMap<u32, u64> = HashMap::new();

                if let Ok(procs) = device.running_compute_processes() {
                    record_process_memory(pid, procs, &mut by_pid);
                }

                if let Ok(procs) = device.running_graphics_processes() {
                    record_process_memory(pid, procs, &mut by_pid);
                }

                total_vram_bytes += by_pid.values().sum::<u64>();
            }
        }

        if total_vram_bytes > 0 {
            Some(bytes_to_mib_ceil(total_vram_bytes))
        } else {
            None
        }
    }

    /// Sample aggregate device-wide VRAM usage across all visible NVIDIA GPUs.
    ///
    /// Unlike per-process sampling, this includes memory used by processes not
    /// managed by llamesh and driver bookkeeping memory. Returns None when NVML
    /// is unavailable or no device memory can be sampled.
    pub fn sample_device_vram(&self) -> Option<GpuMemorySnapshot> {
        #[cfg(test)]
        if let Some(snapshot) = self.device_vram_override.lock().unwrap().clone() {
            return Some(snapshot);
        }

        let nvml = self.nvml.as_ref()?;
        let device_count = nvml.device_count().ok()?;

        let mut devices = Vec::new();
        for index in 0..device_count {
            let Ok(device) = nvml.device_by_index(index) else {
                continue;
            };
            let Ok(info) = device.memory_info() else {
                continue;
            };

            devices.push(GpuDeviceMemory {
                index,
                used_mb: bytes_to_mib_ceil(info.used),
                free_mb: bytes_to_mib_floor(info.free),
                total_mb: bytes_to_mib_floor(info.total),
            });
        }

        if devices.is_empty() {
            return None;
        }

        Some(GpuMemorySnapshot {
            used_mb: devices.iter().map(|d| d.used_mb).sum(),
            free_mb: devices.iter().map(|d| d.free_mb).sum(),
            total_mb: devices.iter().map(|d| d.total_mb).sum(),
            devices,
        })
    }

    /// Sample both VRAM and system memory for a process.
    /// Returns (vram_mb, sysmem_mb), with 0 for unavailable values.
    pub fn sample(&self, pid: u32) -> (u64, u64) {
        let vram = self.sample_vram(pid).unwrap_or(0);
        let sysmem = self.sample_sysmem(pid).unwrap_or(0);
        (vram, sysmem)
    }
}

fn record_process_memory(
    target_pid: u32,
    processes: Vec<nvml_wrapper::struct_wrappers::device::ProcessInfo>,
    by_pid: &mut HashMap<u32, u64>,
) {
    for process in processes {
        if process.pid != target_pid {
            continue;
        }
        if let UsedGpuMemory::Used(mem) = process.used_gpu_memory {
            by_pid
                .entry(process.pid)
                .and_modify(|existing| *existing = (*existing).max(mem))
                .or_insert(mem);
        }
    }
}

impl Default for MemorySampler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_new_does_not_panic() {
        // MemorySampler::new() should not panic even without NVML
        let _sampler = MemorySampler::new();
    }

    #[test]
    fn test_default_same_as_new() {
        let new_sampler = MemorySampler::new();
        let default_sampler = MemorySampler::default();
        // Both should have same nvml state (Some or None based on hardware)
        assert_eq!(new_sampler.nvml.is_some(), default_sampler.nvml.is_some());
    }

    #[test]
    fn test_sample_nonexistent_pid() {
        let sampler = MemorySampler::new();
        // PID 0 or very high PIDs should return (0, 0)
        let (vram, sysmem) = sampler.sample(u32::MAX);
        assert_eq!(vram, 0);
        assert_eq!(sysmem, 0);
    }

    #[test]
    fn test_sample_sysmem_nonexistent() {
        let sampler = MemorySampler::new();
        assert!(sampler.sample_sysmem(u32::MAX).is_none());
    }

    #[test]
    fn test_sample_vram_nonexistent() {
        let sampler = MemorySampler::new();
        // Should return None for non-existent PID (or if no NVML)
        let result = sampler.sample_vram(u32::MAX);
        assert!(result.is_none());
    }

    #[test]
    fn test_sample_current_process() {
        let sampler = MemorySampler::new();
        let pid = std::process::id();
        // Current process should have some memory usage
        let sysmem = sampler.sample_sysmem(pid);
        // sysmem might be None if /proc isn't accessible, but shouldn't panic
        if let Some(mem) = sysmem {
            assert!(mem > 0, "Current process should use some memory");
        }
    }

    #[test]
    fn test_mib_conversion_rounding() {
        assert_eq!(bytes_to_mib_floor(0), 0);
        assert_eq!(bytes_to_mib_floor(BYTES_PER_MIB - 1), 0);
        assert_eq!(bytes_to_mib_floor(BYTES_PER_MIB), 1);
        assert_eq!(bytes_to_mib_ceil(0), 0);
        assert_eq!(bytes_to_mib_ceil(1), 1);
        assert_eq!(bytes_to_mib_ceil(BYTES_PER_MIB), 1);
        assert_eq!(bytes_to_mib_ceil(BYTES_PER_MIB + 1), 2);
    }

    #[test]
    fn test_sample_device_vram_does_not_panic() {
        let sampler = MemorySampler::new();
        let _ = sampler.sample_device_vram();
    }
}
