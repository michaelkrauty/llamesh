use nvml_wrapper::enums::device::UsedGpuMemory;
use nvml_wrapper::Nvml;
use procfs::process::Process;
use tracing::{debug, warn};

/// Samples actual memory usage (VRAM and system memory) from running processes.
pub struct MemorySampler {
    nvml: Option<Nvml>,
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
        Self { nvml }
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
                if let Ok(procs) = device.running_compute_processes() {
                    for proc in procs {
                        if proc.pid == pid {
                            // used_gpu_memory is an enum, extract the value
                            if let UsedGpuMemory::Used(mem) = proc.used_gpu_memory {
                                total_vram_bytes += mem;
                            }
                        }
                    }
                }
            }
        }

        if total_vram_bytes > 0 {
            Some(total_vram_bytes / (1024 * 1024)) // Convert to MiB
        } else {
            None
        }
    }

    /// Sample both VRAM and system memory for a process.
    /// Returns (vram_mb, sysmem_mb), with 0 for unavailable values.
    pub fn sample(&self, pid: u32) -> (u64, u64) {
        let vram = self.sample_vram(pid).unwrap_or(0);
        let sysmem = self.sample_sysmem(pid).unwrap_or(0);
        (vram, sysmem)
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
}
