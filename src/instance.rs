use anyhow::{anyhow, Context, Result};
use parking_lot::Mutex;
use regex::Regex;
use serde::Serialize;
use sha2::{Digest, Sha256};
use std::sync::LazyLock;
use std::process::Stdio;
use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::process::{Child, Command};
use tokio::sync::Notify;
use tracing::{info, warn};

use tokio::io::{AsyncBufReadExt, BufReader};

/// Model parameters parsed from llama-server startup log.
/// This is the ground truth for model capabilities.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ParsedModelParams {
    // Core context/batch
    pub n_ctx_train: Option<u64>,
    pub n_batch: Option<u64>,
    pub n_ubatch: Option<u64>,

    // Model architecture
    pub arch: Option<String>,
    pub n_layer: Option<u64>,
    pub n_embd: Option<u64>,
    pub n_head: Option<u64>,
    pub n_head_kv: Option<u64>,
    pub n_vocab: Option<u64>,
    pub n_ff: Option<u64>,

    // Model identity
    pub model_name: Option<String>,
    pub model_type: Option<String>,
    pub model_params: Option<String>,
    pub file_type: Option<String>,

    // Rope/attention
    pub rope_type: Option<String>,
    pub n_ctx_orig_yarn: Option<u64>,
    pub freq_base_train: Option<f64>,
}

/// Static compiled regex for parsing llama-server log lines.
static LOG_REGEX: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"print_info:\s+(\S+)\s+=\s+(.+)").unwrap());

/// Maximum number of startup log lines to buffer per instance.
/// Prevents unbounded memory growth if an instance floods stdout before becoming ready.
const MAX_STARTUP_LOG_LINES: usize = 10_000;

/// Parse llama-server startup log lines to extract model parameters.
pub fn parse_llama_server_log(lines: &[String]) -> ParsedModelParams {
    let mut params = ParsedModelParams::default();

    let re = &*LOG_REGEX;

    for line in lines {
        if let Some(caps) = re.captures(line) {
            let key = caps.get(1).map(|m| m.as_str()).unwrap_or("");
            let value = caps.get(2).map(|m| m.as_str().trim()).unwrap_or("");

            match key {
                "n_ctx_train" => params.n_ctx_train = value.parse().ok(),
                "n_layer" => params.n_layer = value.parse().ok(),
                "n_embd" => params.n_embd = value.parse().ok(),
                "n_head" => params.n_head = value.parse().ok(),
                "n_head_kv" => params.n_head_kv = value.parse().ok(),
                "n_vocab" => params.n_vocab = value.parse().ok(),
                "n_ff" => params.n_ff = value.parse().ok(),
                "arch" => params.arch = Some(value.to_string()),
                "general.name" => params.model_name = Some(value.to_string()),
                "model" if value.contains("type") => {
                    // "model type" key has a space, handle specially
                }
                "rope" if value.contains("type") => {
                    // "rope type" has a space too
                }
                "n_ctx_orig_yarn" => params.n_ctx_orig_yarn = value.parse().ok(),
                "freq_base_train" => params.freq_base_train = value.parse().ok(),
                _ => {}
            }
        }

        // Handle keys with spaces like "model type", "file type", "rope type"
        if line.contains("print_info: model type") {
            if let Some(idx) = line.find('=') {
                params.model_type = Some(line[idx + 1..].trim().to_string());
            }
        } else if line.contains("print_info: file type") {
            if let Some(idx) = line.find('=') {
                params.file_type = Some(line[idx + 1..].trim().to_string());
            }
        } else if line.contains("print_info: rope type") {
            if let Some(idx) = line.find('=') {
                params.rope_type = Some(line[idx + 1..].trim().to_string());
            }
        } else if line.contains("print_info: model params") {
            if let Some(idx) = line.find('=') {
                params.model_params = Some(line[idx + 1..].trim().to_string());
            }
        }
    }

    params
}

/// Compute a deterministic hash of llama-server launch args.
/// Filters out --host and --port (vary per instance, irrelevant to memory).
pub fn compute_args_hash(args: &[String]) -> String {
    let mut filtered: Vec<&str> = Vec::new();
    let mut skip_next = false;

    for arg in args.iter() {
        if skip_next {
            skip_next = false;
            continue;
        }
        // Skip --host and --port as they vary per instance and don't affect memory
        if arg == "--host" || arg == "--port" {
            skip_next = true;
            continue;
        }
        if arg.starts_with("--host=") || arg.starts_with("--port=") {
            continue;
        }
        filtered.push(arg);
    }

    // Sort for determinism (args order shouldn't matter)
    filtered.sort();

    // Use SHA-256 for a stable hash that won't change across Rust versions.
    // DefaultHasher is not guaranteed to be stable across toolchain upgrades,
    // and args_hash is persisted in metrics snapshots for learned memory data.
    let mut hash = Sha256::new();
    for arg in &filtered {
        hash.update(arg.as_bytes());
        hash.update(b"\0"); // separator to avoid collisions
    }
    let result = hash.finalize();
    format!(
        "{:016x}",
        u64::from_be_bytes(result[..8].try_into().unwrap())
    )
}

#[derive(Debug, Clone, PartialEq)]
pub enum InstanceStatus {
    Starting,
    Ready,
    Failed,
}

#[derive(Debug)]
pub struct Instance {
    pub id: String,
    pub model_name: String,
    pub profile_id: String,
    pub host: String,
    pub port: u16,
    pub start_time: Instant,
    pub last_activity: Instant,
    pub child: Mutex<Option<Child>>,
    pub in_flight_requests: usize,
    pub status: Mutex<InstanceStatus>,
    pub ready_signal: Arc<Notify>,
    /// Hash of launch args (excluding --host/--port) for learned memory lookup.
    pub args_hash: String,
    /// Buffered startup log lines for parsing model parameters.
    pub startup_log: Arc<Mutex<Vec<String>>>,
    /// Parsed model parameters from startup log (populated after ready).
    pub parsed_params: Mutex<Option<ParsedModelParams>>,
    /// Whether this is a cold start (no learned memory data for this args_hash).
    pub is_cold_start: bool,
    /// Whether this instance is draining (no new requests should be assigned).
    pub draining: AtomicBool,
}

impl Drop for Instance {
    fn drop(&mut self) {
        let mut guard = self.child.lock();
        if let Some(mut child) = guard.take() {
            // First try synchronous kill (non-blocking signal)
            let _ = child.start_kill();
            // Then spawn async wait if runtime available, to reap the zombie
            if let Ok(handle) = tokio::runtime::Handle::try_current() {
                handle.spawn(async move {
                    // Wait for process to exit after kill signal, with timeout
                    let _ = tokio::time::timeout(Duration::from_secs(5), child.wait()).await;
                });
            } else {
                // Fallback: spawn blocking thread to reap process when no runtime
                std::thread::spawn(move || {
                    for _ in 0..50 {
                        // 5 seconds total (50 * 100ms)
                        if let Ok(Some(_)) = child.try_wait() {
                            return;
                        }
                        std::thread::sleep(Duration::from_millis(100));
                    }
                    // Force kill if still running after timeout
                    let _ = child.start_kill();
                });
            }
        }
    }
}

impl Instance {
    pub fn new(
        id: String,
        model: String,
        profile: String,
        host: String,
        port: u16,
        args_hash: String,
        is_cold_start: bool,
    ) -> Self {
        Self {
            id,
            model_name: model,
            profile_id: profile,
            host,
            port,
            start_time: Instant::now(),
            last_activity: Instant::now(),
            child: Mutex::new(None),
            in_flight_requests: 0,
            status: Mutex::new(InstanceStatus::Starting),
            ready_signal: Arc::new(Notify::new()),
            args_hash,
            startup_log: Arc::new(Mutex::new(Vec::new())),
            parsed_params: Mutex::new(None),
            is_cold_start,
            draining: AtomicBool::new(false),
        }
    }

    /// Get the PID of the child process, if running.
    pub fn get_pid(&self) -> Option<u32> {
        let guard = self.child.lock();
        guard.as_ref()?.id()
    }

    // Spawn only starts the process, does not wait for readiness
    pub fn spawn(&self, binary_path: &str, args: &[String]) -> Result<()> {
        info!(
            "Spawning instance {} for {} on {}:{} with args: {:?}",
            self.id, self.model_name, self.host, self.port, args
        );

        let mut cmd = Command::new(binary_path);
        cmd.args(args);

        // Set LD_LIBRARY_PATH to the directory containing the resolved binary so
        // shared libraries (libllama.so, libggml.so, libmtmd.so, etc.) that live
        // alongside the binary can be found by the dynamic linker at runtime.
        // The binary_path is often a symlink (e.g., build/bin/llama-server ->
        // build-<hash>/bin/llama-server), so we resolve it first.
        if let Ok(resolved) = std::fs::canonicalize(binary_path) {
            if let Some(parent) = resolved.parent() {
                let lib_dir = parent.to_string_lossy();
                let existing = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
                let new_path = if existing.is_empty() {
                    lib_dir.to_string()
                } else {
                    format!("{}:{}", lib_dir, existing)
                };
                cmd.env("LD_LIBRARY_PATH", new_path);
            }
        }

        // Pipe output to capture logs
        cmd.stdout(Stdio::piped());
        cmd.stderr(Stdio::piped());

        let mut child = cmd.spawn().context("Failed to spawn llama-server")?;

        // Spawn tasks to stream stdout/stderr to tracing and buffer for parsing
        if let Some(stdout) = child.stdout.take() {
            let id = self.id.clone();
            let startup_log = self.startup_log.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stdout).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    info!(instance_id = %id, "stdout: {}", line);
                    let mut log = startup_log.lock();
                    if log.len() < MAX_STARTUP_LOG_LINES {
                        log.push(line);
                    }
                }
            });
        }

        if let Some(stderr) = child.stderr.take() {
            let id = self.id.clone();
            let startup_log = self.startup_log.clone();
            tokio::spawn(async move {
                let mut reader = BufReader::new(stderr).lines();
                while let Ok(Some(line)) = reader.next_line().await {
                    info!(instance_id = %id, "stderr: {}", line);
                    let mut log = startup_log.lock();
                    if log.len() < MAX_STARTUP_LOG_LINES {
                        log.push(line);
                    }
                }
            });
        }

        {
            let mut guard = self.child.lock();
            *guard = Some(child);
        }

        Ok(())
    }

    /// Parse the buffered startup log and store the result.
    /// Call this after wait_for_ready() succeeds.
    pub fn parse_and_store_startup_params(&self) {
        let lines = self.startup_log.lock();
        let params = parse_llama_server_log(&lines);

        // Check if we got any meaningful data
        let has_data =
            params.n_ctx_train.is_some() || params.arch.is_some() || params.model_name.is_some();

        if has_data {
            info!(
                instance_id = %self.id,
                "Parsed model params: arch={:?}, n_ctx_train={:?}, n_layer={:?}, model_name={:?}",
                params.arch, params.n_ctx_train, params.n_layer, params.model_name
            );
            *self.parsed_params.lock() = Some(params);
        } else {
            warn!(
                instance_id = %self.id,
                "Failed to parse model params from startup log ({} lines buffered)",
                lines.len()
            );
        }
    }

    /// Clear the startup log buffer to free memory.
    pub fn clear_startup_log(&self) {
        self.startup_log.lock().clear();
    }

    /// Get parsed model parameters, if available.
    pub fn get_parsed_params(&self) -> Option<ParsedModelParams> {
        self.parsed_params.lock().clone()
    }

    pub async fn wait_for_ready(
        &self,
        startup_timeout_secs: u64,
        api_key: Option<String>,
        client: &reqwest::Client,
    ) -> Result<()> {
        let host = &self.host;
        let check_host = if host == "0.0.0.0" || host == "::" {
            "127.0.0.1"
        } else {
            host
        };

        let health_addr = format!("http://{}:{}/health", check_host, self.port);
        let start = Instant::now();

        // Enforce minimum timeout to prevent infinite wait
        const MIN_STARTUP_TIMEOUT_SECS: u64 = 30;
        let effective_timeout = if startup_timeout_secs == 0 {
            MIN_STARTUP_TIMEOUT_SECS
        } else {
            startup_timeout_secs
        };
        let timeout = Duration::from_secs(effective_timeout);

        loop {
            if start.elapsed() >= timeout {
                break;
            }

            if !self.is_alive() {
                {
                    let mut status = self.status.lock();
                    *status = InstanceStatus::Failed;
                }
                self.ready_signal.notify_waiters();
                return Err(anyhow!(
                    "Instance process exited unexpectedly while waiting for readiness"
                ));
            }

            // Only use /health endpoint to determine readiness.
            // The /health endpoint returns 503 while the model is loading and 200 when ready.
            // Note: /v1/models can return 200 even while model is still loading, so it's not
            // a reliable indicator of readiness for serving requests.
            let mut health_req = client.get(&health_addr);
            if let Some(ref key) = api_key {
                health_req = health_req.header("Authorization", format!("Bearer {}", key));
            }

            match health_req.send().await {
                Ok(resp) => {
                    if resp.status().is_success() {
                        self.mark_ready();
                        return Ok(());
                    }
                    // 503 means model is still loading, keep polling
                }
                Err(_) => {
                    // Connection refused or timeout - server might not be up yet, keep polling
                }
            }

            tokio::time::sleep(Duration::from_millis(500)).await;
        }

        {
            let mut status = self.status.lock();
            *status = InstanceStatus::Failed;
        }
        self.ready_signal.notify_waiters();
        Err(anyhow!("Instance failed to become ready within timeout"))
    }

    fn mark_ready(&self) {
        let mut status = self.status.lock();
        *status = InstanceStatus::Ready;
        self.ready_signal.notify_waiters();
    }

    pub async fn stop(&self) -> Result<()> {
        let child_opt = {
            let mut guard = self.child.lock();
            guard.take()
        };

        if let Some(mut child) = child_opt {
            info!(
                "Stopping instance {} (up for {:?})",
                self.id,
                self.start_time.elapsed()
            );

            #[cfg(unix)]
            {
                use nix::sys::signal::{self, Signal};
                use nix::unistd::Pid;

                if let Some(pid) = child.id() {
                    let _ = signal::kill(Pid::from_raw(pid as i32), Signal::SIGTERM);
                }

                // Wait for graceful exit
                match tokio::time::timeout(Duration::from_secs(10), child.wait()).await {
                    Ok(_) => return Ok(()),
                    Err(_) => {
                        warn!("Instance {} did not stop gracefully; forcing kill", self.id);
                    }
                }
            }

            child.kill().await?;
            child.wait().await?;
        }
        Ok(())
    }

    pub fn is_alive(&self) -> bool {
        let mut guard = self.child.lock();

        match &mut *guard {
            Some(child) => match child.try_wait() {
                Ok(Some(status)) => {
                    warn!(
                        instance_id = %self.id,
                        model = %self.model_name,
                        profile = %self.profile_id,
                        exit_status = ?status,
                        "Instance exited unexpectedly"
                    );
                    *guard = None;
                    false
                }
                Ok(None) => true,
                Err(err) => {
                    warn!(
                        instance_id = %self.id,
                        model = %self.model_name,
                        profile = %self.profile_id,
                        error = %err,
                        "Failed to poll child status; assuming instance is dead"
                    );
                    *guard = None;
                    false
                }
            },
            None => false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    // Compile-time validation of timeout constant bounds
    const _: () = {
        const MIN_STARTUP_TIMEOUT_SECS: u64 = 30;
        assert!(MIN_STARTUP_TIMEOUT_SECS >= 10, "Minimum timeout should be at least 10 seconds");
        assert!(MIN_STARTUP_TIMEOUT_SECS <= 120, "Minimum timeout should be at most 120 seconds");
    };

    #[test]
    fn test_instance_status_transitions() {
        // Test that InstanceStatus can be cloned and compared
        let status = InstanceStatus::Starting;
        assert_eq!(status, InstanceStatus::Starting);

        let status = InstanceStatus::Ready;
        assert_eq!(status, InstanceStatus::Ready);

        let status = InstanceStatus::Failed;
        assert_eq!(status, InstanceStatus::Failed);
    }

    #[test]
    fn test_instance_creation() {
        let instance = Instance::new(
            "test-id".to_string(),
            "test-model".to_string(),
            "default".to_string(),
            "127.0.0.1".to_string(),
            8080,
            "abc123".to_string(),
            false,
        );

        assert_eq!(instance.id, "test-id");
        assert_eq!(instance.model_name, "test-model");
        assert_eq!(instance.profile_id, "default");
        assert_eq!(instance.host, "127.0.0.1");
        assert_eq!(instance.port, 8080);
        assert_eq!(instance.in_flight_requests, 0);
        assert_eq!(instance.args_hash, "abc123");
        assert!(!instance.is_cold_start);
        assert!(!instance.draining.load(Ordering::Relaxed));

        // Verify mutex works (parking_lot doesn't panic)
        let status = instance.status.lock().clone();
        assert_eq!(status, InstanceStatus::Starting);
    }

    #[test]
    fn test_compute_args_hash_determinism() {
        // Same args should produce same hash
        let args1 = vec![
            "-m".to_string(),
            "/models/foo.gguf".to_string(),
            "-c".to_string(),
            "4096".to_string(),
        ];
        let args2 = vec![
            "-m".to_string(),
            "/models/foo.gguf".to_string(),
            "-c".to_string(),
            "4096".to_string(),
        ];
        assert_eq!(compute_args_hash(&args1), compute_args_hash(&args2));

        // Different args should produce different hash
        let args3 = vec![
            "-m".to_string(),
            "/models/foo.gguf".to_string(),
            "-c".to_string(),
            "8192".to_string(),
        ];
        assert_ne!(compute_args_hash(&args1), compute_args_hash(&args3));
    }

    #[test]
    fn test_compute_args_hash_filters_host_port() {
        let args_with_host_port = vec![
            "-m".to_string(),
            "/models/foo.gguf".to_string(),
            "--host".to_string(),
            "0.0.0.0".to_string(),
            "--port".to_string(),
            "8080".to_string(),
        ];
        let args_without = vec!["-m".to_string(), "/models/foo.gguf".to_string()];

        // Should produce same hash since --host and --port are filtered out
        assert_eq!(
            compute_args_hash(&args_with_host_port),
            compute_args_hash(&args_without)
        );
    }

    #[test]
    fn test_compute_args_hash_order_independent() {
        // Args in different order should produce same hash (after sorting)
        let args1 = vec![
            "-c".to_string(),
            "4096".to_string(),
            "-m".to_string(),
            "/models/foo.gguf".to_string(),
        ];
        let args2 = vec![
            "-m".to_string(),
            "/models/foo.gguf".to_string(),
            "-c".to_string(),
            "4096".to_string(),
        ];
        assert_eq!(compute_args_hash(&args1), compute_args_hash(&args2));
    }

    #[test]
    fn test_parking_lot_mutex_no_poison() {
        // parking_lot::Mutex doesn't have poisoning behavior at all
        // This test verifies we can acquire lock, release it, and acquire again
        // (std::sync::Mutex would require handling PoisonError)

        let instance = Instance::new(
            "test-id".to_string(),
            "test-model".to_string(),
            "default".to_string(),
            "127.0.0.1".to_string(),
            8080,
            "hash123".to_string(),
            false,
        );

        // Acquire and release lock multiple times
        {
            let mut status = instance.status.lock();
            *status = InstanceStatus::Ready;
        }

        {
            let status = instance.status.lock();
            assert_eq!(*status, InstanceStatus::Ready);
        }

        // With parking_lot, .lock() returns the guard directly
        // No need for .unwrap() or error handling
        let mut status = instance.status.lock();
        *status = InstanceStatus::Failed;
        drop(status);

        let status = instance.status.lock();
        assert_eq!(*status, InstanceStatus::Failed);
    }

    #[test]
    fn test_get_pid_returns_none_before_spawn() {
        let instance = Instance::new(
            "test-id".to_string(),
            "test-model".to_string(),
            "default".to_string(),
            "127.0.0.1".to_string(),
            8080,
            "hash123".to_string(),
            false,
        );
        // Before spawn, get_pid should return None
        assert!(instance.get_pid().is_none());
    }

    #[test]
    fn test_clear_startup_log() {
        let instance = Instance::new(
            "test-id".to_string(),
            "test-model".to_string(),
            "default".to_string(),
            "127.0.0.1".to_string(),
            8080,
            "hash123".to_string(),
            false,
        );

        // Add some data to the startup log
        instance
            .startup_log
            .lock()
            .push("test log data".to_string());
        assert!(!instance.startup_log.lock().is_empty());

        // Clear it
        instance.clear_startup_log();
        assert!(instance.startup_log.lock().is_empty());
    }

    #[test]
    fn test_get_parsed_params_default_none() {
        let instance = Instance::new(
            "test-id".to_string(),
            "test-model".to_string(),
            "default".to_string(),
            "127.0.0.1".to_string(),
            8080,
            "hash123".to_string(),
            false,
        );
        // Before parsing, should be None
        assert!(instance.get_parsed_params().is_none());
    }

    #[test]
    fn test_parse_llama_server_log_extracts_params() {
        let lines = vec![
            "print_info: n_ctx_train = 32768".to_string(),
            "print_info: n_vocab = 128256".to_string(),
            "print_info: arch = llama".to_string(),
        ];
        let params = parse_llama_server_log(&lines);
        assert_eq!(params.n_ctx_train, Some(32768));
        assert_eq!(params.n_vocab, Some(128256));
        assert_eq!(params.arch, Some("llama".to_string()));
    }

    #[test]
    fn test_parse_llama_server_log_empty() {
        let lines: Vec<String> = vec!["no matching lines here".to_string()];
        let params = parse_llama_server_log(&lines);
        // All fields should be None
        assert!(params.n_ctx_train.is_none());
        assert!(params.arch.is_none());
    }

    #[test]
    fn test_parsed_model_params_default() {
        let params = ParsedModelParams::default();
        assert!(params.n_ctx_train.is_none());
        assert!(params.n_layer.is_none());
        assert!(params.arch.is_none());
        assert!(params.model_name.is_none());
    }
}
