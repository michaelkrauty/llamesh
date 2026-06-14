use crate::config::LlamaCppConfig;
use anyhow::{anyhow, Result};
use parking_lot::{Mutex, RwLock};
use serde::Serialize;
use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncRead, AsyncWriteExt, BufReader};
use tokio::process::Command;
use tracing::{info, warn};

/// Guard that clears the building flag when dropped (on success or error)
struct BuildGuard {
    building: Arc<AtomicBool>,
}

impl Drop for BuildGuard {
    fn drop(&mut self) {
        self.building.store(false, Ordering::Relaxed);
    }
}

/// Cache entry for binary existence check
struct BinaryExistsCache {
    exists: bool,
    checked_at: Instant,
}

// 1s TTL for faster health check response after builds complete
const BINARY_CACHE_TTL: Duration = Duration::from_secs(1);

/// Snapshot of the current build status, suitable for JSON serialization.
#[derive(Clone, Serialize)]
pub struct BuildStatus {
    pub is_building: bool,
    pub last_build_error: Option<String>,
    pub last_build_at: Option<String>,
}

#[derive(Clone)]
pub struct BuildManager {
    config: LlamaCppConfig,
    current_version: Arc<RwLock<Option<String>>>,
    building: Arc<AtomicBool>,
    binary_exists_cache: Arc<RwLock<Option<BinaryExistsCache>>>,
    /// Monotonically increasing generation counter, incremented on each binary swap.
    /// Subscribers can poll this to detect when a new binary is available.
    binary_generation: Arc<AtomicU64>,
    /// Notify listeners when binary_generation changes (binary swap occurred).
    binary_swap_notify: Arc<tokio::sync::Notify>,
    last_build_error: Arc<RwLock<Option<String>>>,
    last_build_at: Arc<RwLock<Option<String>>>,
}

impl BuildManager {
    pub fn new(config: LlamaCppConfig) -> Self {
        Self {
            config,
            current_version: Arc::new(RwLock::new(None)),
            building: Arc::new(AtomicBool::new(false)),
            binary_exists_cache: Arc::new(RwLock::new(None)),
            binary_generation: Arc::new(AtomicU64::new(0)),
            binary_swap_notify: Arc::new(tokio::sync::Notify::new()),
            last_build_error: Arc::new(RwLock::new(None)),
            last_build_at: Arc::new(RwLock::new(None)),
        }
    }

    /// Returns the current binary generation counter.
    pub fn binary_generation(&self) -> u64 {
        self.binary_generation.load(Ordering::Acquire)
    }

    /// Returns a reference to the Notify used to signal binary swaps.
    pub fn binary_swap_notify(&self) -> &Arc<tokio::sync::Notify> {
        &self.binary_swap_notify
    }

    /// Returns true if a build is currently in progress
    pub fn is_building(&self) -> bool {
        self.building.load(Ordering::Relaxed)
    }

    /// Returns true if the binary exists and is usable (uncached)
    pub fn is_binary_available(&self) -> bool {
        Path::new(&self.config.binary_path).exists()
    }

    /// Returns cached binary existence check (1s TTL)
    /// Reduces filesystem stat calls for high-frequency health checks
    pub fn is_binary_available_cached(&self) -> bool {
        // Check cache first
        {
            let cache = self.binary_exists_cache.read();
            if let Some(ref entry) = *cache {
                if entry.checked_at.elapsed() < BINARY_CACHE_TTL {
                    return entry.exists;
                }
            }
        }

        // Cache miss or stale - do actual check
        let exists = Path::new(&self.config.binary_path).exists();
        {
            let mut cache = self.binary_exists_cache.write();
            *cache = Some(BinaryExistsCache {
                exists,
                checked_at: Instant::now(),
            });
        }
        exists
    }

    /// Invalidates the binary existence cache (call after builds)
    pub fn invalidate_binary_cache(&self) {
        let mut cache = self.binary_exists_cache.write();
        *cache = None;
    }

    /// Returns true if the node can serve requests locally (binary exists).
    /// Builds happen in isolated per-commit directories and the existing binary
    /// symlink remains valid throughout, so building does not affect serving.
    pub fn can_serve(&self) -> bool {
        self.is_binary_available()
    }

    /// Returns a snapshot of the current build status.
    pub fn build_status(&self) -> BuildStatus {
        BuildStatus {
            is_building: self.is_building(),
            last_build_error: self.last_build_error.read().clone(),
            last_build_at: self.last_build_at.read().clone(),
        }
    }

    /// Record a successful build completion.
    fn record_build_success(&self) {
        *self.last_build_error.write() = None;
        *self.last_build_at.write() = Some(chrono::Utc::now().to_rfc3339());
    }

    /// Record a build failure.
    fn record_build_failure(&self, error: &str) {
        *self.last_build_error.write() = Some(error.to_string());
        *self.last_build_at.write() = Some(chrono::Utc::now().to_rfc3339());
    }

    pub async fn get_version(&self) -> String {
        self.current_version
            .read()
            .clone()
            .unwrap_or_else(|| "unknown".to_string())
    }

    /// Record the llama.cpp commit that is now the live binary. Called only
    /// after `update_symlink` succeeds, so `get_version()` reflects the binary
    /// that is actually serving — never a commit that is still building, and
    /// never one whose build later failed (which would otherwise leave the
    /// reported version pinned to a commit that never ran).
    fn record_running_version(&self, commit: &str) {
        *self.current_version.write() = Some(commit.to_string());
    }

    pub async fn update_and_build(&self) -> Result<()> {
        let result = self.update_and_build_inner().await;
        match &result {
            Ok(()) => self.record_build_success(),
            Err(e) => self.record_build_failure(&e.to_string()),
        }
        result
    }

    async fn update_and_build_inner(&self) -> Result<()> {
        if !self.config.enabled {
            info!("Build disabled in config. Skipping llama.cpp update/build.");
            return Ok(());
        }

        // Note: building flag is set later, only during actual compilation
        // Git operations and binary verification don't block serving requests

        info!("Checking for llama.cpp updates...");

        let repo_path = Path::new(&self.config.repo_path);
        if !repo_path.exists() {
            info!("Cloning llama.cpp from {}", self.config.repo_url);
            run_command(
                Command::new("git")
                    .arg("clone")
                    .arg(&self.config.repo_url)
                    .arg(repo_path),
            )
            .await?;
        }

        // Fetch and checkout
        info!("Updating repo on branch/tag {}...", self.config.branch);

        // Fetch all branches and tags
        run_command(
            Command::new("git")
                .current_dir(repo_path)
                .arg("fetch")
                .arg("--all")
                .arg("--tags"),
        )
        .await?;

        // Check if the ref is a tag
        let is_tag = Command::new("git")
            .current_dir(repo_path)
            .arg("tag")
            .arg("-l")
            .arg(&self.config.branch)
            .output()
            .await
            .map(|output| !output.stdout.is_empty())
            .unwrap_or(false);

        run_command(
            Command::new("git")
                .current_dir(repo_path)
                .arg("checkout")
                .arg(&self.config.branch),
        )
        .await?;

        // For branches, reset to origin. For tags/commits, the checkout is sufficient.
        if !is_tag {
            // Try reset for branches - if it fails (e.g., it's a commit hash), that's fine
            let reset_result = run_command(
                Command::new("git")
                    .current_dir(repo_path)
                    .arg("reset")
                    .arg("--hard")
                    .arg(format!("origin/{}", self.config.branch)),
            )
            .await;

            if reset_result.is_err() {
                info!("Reset to origin/{} failed (likely a commit hash or detached ref), using checkout result", self.config.branch);
            }
        } else {
            info!(
                "Detected tag {}, skipping reset to origin",
                self.config.branch
            );
        }

        // Get current commit hash
        let output = Command::new("git")
            .current_dir(repo_path)
            .arg("rev-parse")
            .arg("--short")
            .arg("HEAD")
            .output()
            .await?;

        if !output.status.success() {
            return Err(anyhow!("Failed to get git commit hash"));
        }
        let commit_hash = String::from_utf8_lossy(&output.stdout).trim().to_string();
        info!("Current llama.cpp commit: {}", commit_hash);

        info!(event = "llama_build_start", branch = %self.config.branch, commit = %commit_hash);

        // Determine unique build directory for this commit
        // Assuming config.build_path is a base like "./llama.cpp/build"
        // We make "./llama.cpp/build-<commit>"
        let build_base = PathBuf::from(&self.config.build_path);
        // If build_path ends in "build", strip it to get parent?
        // Actually, let's just append the hash to the configured path to avoid confusion.
        // If config.build_path is "./llama.cpp/build", we use "./llama.cpp/build-<hash>"
        let base_name = build_base
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                anyhow!(
                    "build_path has no valid directory name: {}",
                    build_base.display()
                )
            })?;
        let build_dir_name = format!("{base_name}-{commit_hash}");
        let build_dir = build_base
            .parent()
            .unwrap_or(Path::new("."))
            .join(build_dir_name);

        // Ideally we find where the binary *will* be.
        // Standard llama.cpp cmake: <build_dir>/bin/llama-server
        let actual_binary_path = build_dir.join("bin").join("llama-server");

        if actual_binary_path.exists() {
            info!(
                "Binary for commit {} already exists. Verifying...",
                commit_hash
            );
            if self.verify_binary(&actual_binary_path).await.is_ok() {
                // If the live symlink already points at this binary there is
                // nothing to swap: signaling a swap anyway would drain every
                // running instance for a binary change that never happened
                // (the common case for the scheduled update check when
                // upstream has no new commits, and for every restart).
                // Builds land in immutable per-commit directories, so an
                // identical resolved path means running instances already
                // use exactly this binary. A rebuild into the same directory
                // (the verification-failure path below) does not take this
                // branch and still signals a swap.
                if self.symlink_already_current(&actual_binary_path).await {
                    info!("Existing binary verified and already live. Skipping binary swap.");
                    self.record_running_version(&commit_hash);
                    return Ok(());
                }
                info!("Existing binary verified. Switching symlink.");
                self.update_symlink(&actual_binary_path).await?;
                self.record_running_version(&commit_hash);
                return Ok(());
            }
            warn!("Existing binary verification failed. Rebuilding.");
        }

        // Set building flag NOW - only during actual compilation
        // This is cleared on completion or error via the guard
        self.building.store(true, Ordering::Relaxed);
        let _build_guard = BuildGuard {
            building: self.building.clone(),
        };

        // Build
        info!("Building llama.cpp in {}...", build_dir.display());
        tokio::fs::create_dir_all(&build_dir).await?;

        // We need absolute path to repo for cmake -S if we are in build dir?
        // Or just relative. repo_path is likely relative.
        // cmake -S <source> -B <build>
        let abs_repo_path = tokio::fs::canonicalize(repo_path).await?;
        let abs_build_dir = tokio::fs::canonicalize(&build_dir)
            .await
            .unwrap_or(build_dir.clone()); // Might not exist yet fully if create_dir_all failed? No it succeeded.

        run_command(
            Command::new("cmake")
                .arg("-S")
                .arg(&abs_repo_path)
                .arg("-B")
                .arg(&abs_build_dir)
                .args(&self.config.build_args),
        )
        .await?;

        run_command(
            Command::new("cmake")
                .arg("--build")
                .arg(&abs_build_dir)
                .arg("--config")
                .arg("Release")
                .args(&self.config.build_command_args),
        )
        .await?;

        // Verify binary
        info!("Verifying newly built binary...");
        self.verify_binary(&actual_binary_path).await?;

        // Switch symlink
        self.update_symlink(&actual_binary_path).await?;
        self.record_running_version(&commit_hash);

        // Cleanup old builds
        if let Err(e) = self.cleanup_old_builds().await {
            warn!("Failed to cleanup old builds: {}", e);
        }

        info!(event = "llama_build_success", commit = %commit_hash, binary_path = %self.config.binary_path);
        Ok(())
    }

    async fn cleanup_old_builds(&self) -> Result<()> {
        let build_base = PathBuf::from(&self.config.build_path);
        let parent_dir = build_base.parent().unwrap_or(Path::new("."));
        let base_name = match build_base
            .file_name()
            .and_then(|n| n.to_str())
            .filter(|s| !s.is_empty())
        {
            Some(name) => name,
            None => {
                warn!("build_path has no valid directory name, skipping cleanup");
                return Ok(());
            }
        };
        let prefix = format!("{base_name}-");

        let mut entries = tokio::fs::read_dir(parent_dir).await?;
        let mut builds = Vec::new();

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            if let Some(name) = path.file_name().and_then(|n| n.to_str()) {
                if name.starts_with(&prefix) {
                    if let Ok(metadata) = entry.metadata().await {
                        let time = metadata.created().or_else(|_| metadata.modified());
                        if let Ok(t) = time {
                            builds.push((path, t));
                        }
                    }
                }
            }
        }

        // Sort by creation time (oldest first)
        builds.sort_by_key(|(_, created)| *created);

        // Resolve the build directory the live binary symlink currently points
        // into, so cleanup never deletes the build that is actively serving —
        // even when keep_builds is small (e.g. 0) or a build's timestamp is out
        // of order. The managed layout is `<build-dir>/bin/llama-server`, so the
        // build dir is two levels up from the resolved binary.
        let active_build_dir = tokio::fs::canonicalize(&self.config.binary_path)
            .await
            .ok()
            .and_then(|bin| Some(bin.parent()?.parent()?.to_path_buf()));

        // The active build is always retained. Among the remaining (non-active)
        // builds, keep the `keep_builds` most recent and remove the older ones,
        // so protecting the active build never reduces how many previous builds
        // are kept for rollback — even when the active build is itself old.
        let mut non_active = Vec::new();
        for (path, _) in &builds {
            let is_active = match &active_build_dir {
                Some(active) => {
                    tokio::fs::canonicalize(path).await.ok().as_deref() == Some(active.as_path())
                }
                None => false,
            };
            if !is_active {
                non_active.push(path);
            }
        }

        let keep_count = self.config.keep_builds;
        if non_active.len() > keep_count {
            let remove_upto = non_active.len() - keep_count;
            for &path in &non_active[..remove_upto] {
                info!("Removing old build: {}", path.display());
                if let Err(e) = tokio::fs::remove_dir_all(path).await {
                    warn!("Failed to remove old build {}: {}", path.display(), e);
                }
            }
        }

        Ok(())
    }

    async fn verify_binary(&self, path: &Path) -> Result<()> {
        let mut cmd = Command::new(path);
        cmd.arg("--help");

        // Set LD_LIBRARY_PATH so the binary can find its shared libraries
        // (libllama.so, libggml.so, etc.) which live alongside it in the build dir.
        if let Ok(resolved) = std::fs::canonicalize(path) {
            if let Some(parent) = resolved.parent() {
                let lib_dir = parent.to_string_lossy();
                let existing = std::env::var("LD_LIBRARY_PATH").unwrap_or_default();
                let new_path = if existing.is_empty() {
                    lib_dir.to_string()
                } else {
                    format!("{lib_dir}:{existing}")
                };
                cmd.env("LD_LIBRARY_PATH", new_path);
            }
        }

        let output = cmd.output().await;

        match output {
            Ok(out) if out.status.success() => Ok(()),
            Ok(out) => {
                let stderr = String::from_utf8_lossy(&out.stderr);
                warn!(
                    "Binary verification failed for {}: {}",
                    path.display(),
                    stderr
                );
                Err(anyhow!("Binary verification failed"))
            }
            Err(e) => {
                warn!(
                    "Binary verification failed to execute {}: {}",
                    path.display(),
                    e
                );
                Err(anyhow!("Binary verification failed to execute"))
            }
        }
    }

    /// Returns true when the configured binary path already resolves to the
    /// same file as `target`. Any failure to resolve either side (missing
    /// link, dangling target, non-symlink fallback installs) returns false,
    /// in which case the caller proceeds with a normal swap — the check only
    /// ever skips work, never a needed swap.
    async fn symlink_already_current(&self, target: &Path) -> bool {
        let link_path = Path::new(&self.config.binary_path);
        let (Ok(resolved_link), Ok(resolved_target)) = (
            tokio::fs::canonicalize(link_path).await,
            tokio::fs::canonicalize(target).await,
        ) else {
            return false;
        };
        resolved_link == resolved_target
    }

    async fn update_symlink(&self, target: &Path) -> Result<()> {
        let link_path = Path::new(&self.config.binary_path);

        // Ensure parent dir of link exists
        if let Some(parent) = link_path.parent() {
            tokio::fs::create_dir_all(parent).await?;
        }

        // Create absolute path for target to ensure symlink validity
        let abs_target = if target.is_absolute() {
            target.to_path_buf()
        } else {
            tokio::fs::canonicalize(target)
                .await
                .unwrap_or(target.to_path_buf())
        };

        // Atomically swap: create symlink with unique temp name, then rename
        // Use unique name to avoid TOCTOU race with other processes
        let tmp_link = link_path.with_extension(format!("tmp.{}", std::process::id()));

        // Remove tmp if exists (from previous failed attempt by this process)
        let _ = tokio::fs::remove_file(&tmp_link).await;

        #[cfg(unix)]
        {
            use std::os::unix::fs::symlink;
            // If symlink fails (e.g., file exists from race), retry with different name
            if symlink(&abs_target, &tmp_link).is_err() {
                let tmp_link2 = link_path.with_extension(format!(
                    "tmp.{}.{}",
                    std::process::id(),
                    rand::random::<u32>()
                ));
                let _ = tokio::fs::remove_file(&tmp_link2).await;
                symlink(&abs_target, &tmp_link2)?;
                if let Err(e) = tokio::fs::rename(&tmp_link2, link_path).await {
                    // Clean up temp symlink on rename failure
                    let _ = tokio::fs::remove_file(&tmp_link2).await;
                    return Err(e.into());
                }
                info!(
                    "Updated symlink {} -> {}",
                    link_path.display(),
                    abs_target.display()
                );
                self.invalidate_binary_cache();
                self.signal_binary_swap();
                return Ok(());
            }
        }
        #[cfg(not(unix))]
        {
            // Fallback for non-unix (e.g. windows) - not atomic but works
            if link_path.exists() {
                tokio::fs::remove_file(link_path).await?;
            }
            tokio::fs::copy(&abs_target, link_path).await?;
            return Ok(());
        }

        if let Err(e) = tokio::fs::rename(&tmp_link, link_path).await {
            // Clean up the temp symlink on rename failure
            let _ = tokio::fs::remove_file(&tmp_link).await;
            return Err(e.into());
        }
        info!(
            "Updated symlink {} -> {}",
            link_path.display(),
            abs_target.display()
        );

        // Invalidate binary existence cache after symlink update
        self.invalidate_binary_cache();
        self.signal_binary_swap();

        Ok(())
    }

    /// Increment the binary generation counter and notify waiters.
    /// Called after a successful symlink swap to trigger instance draining.
    fn signal_binary_swap(&self) {
        let gen = self.binary_generation.fetch_add(1, Ordering::Release) + 1;
        info!(
            event = "binary_swap_signaled",
            generation = gen,
            "Binary swap complete, signaling instance drain"
        );
        self.binary_swap_notify.notify_one();
    }
}

/// Number of trailing output lines retained for error reporting.
const COMMAND_OUTPUT_TAIL_LINES: usize = 40;
/// Retained output lines longer than this are truncated, keeping error
/// messages (and the `last_build_error` API field) bounded.
const COMMAND_OUTPUT_MAX_LINE_LEN: usize = 400;
/// Hard cap on bytes buffered for a single output line. Bytes beyond this
/// (up to the next newline) are consumed and discarded, so a child emitting
/// an enormous newline-free line cannot grow the buffer without bound.
const COMMAND_OUTPUT_MAX_LINE_BYTES: usize = 8192;

/// Reads up to and including the next `\n` from `reader`, appending at most
/// `cap` bytes to `buf`. The remainder of an over-long line is consumed and
/// discarded, bounding memory regardless of what the child writes. Returns
/// `Ok(false)` on EOF with nothing read.
async fn read_line_capped<R>(reader: &mut R, buf: &mut Vec<u8>, cap: usize) -> std::io::Result<bool>
where
    R: tokio::io::AsyncBufRead + Unpin,
{
    let mut read_any = false;
    loop {
        let available = reader.fill_buf().await?;
        if available.is_empty() {
            return Ok(read_any);
        }
        read_any = true;
        let (chunk_len, found_newline) = match available.iter().position(|&b| b == b'\n') {
            Some(i) => (i + 1, true),
            None => (available.len(), false),
        };
        let take = chunk_len.min(cap.saturating_sub(buf.len()));
        buf.extend_from_slice(&available[..take]);
        reader.consume(chunk_len);
        if found_newline {
            return Ok(true);
        }
    }
}

/// Derive the llama.cpp commit from a *resolved* managed-build binary path.
///
/// Managed builds live in `<base>-<commit>/bin/llama-server` (the same
/// `<commit>` that `record_running_version` stores), so the commit can be read
/// off the path of the executable a process was actually spawned from —
/// immune to a concurrent symlink swap, and available before the startup
/// build check has recorded a version. Returns `None` for paths that don't
/// match the managed-build layout (e.g. hand-supplied binaries).
pub fn version_from_resolved_binary(path: &std::path::Path) -> Option<String> {
    let bin_dir = path.parent()?;
    let build_dir = bin_dir.parent()?.file_name()?.to_str()?;
    let (_, commit) = build_dir.rsplit_once('-')?;
    let looks_like_commit = commit.len() >= 7 && commit.chars().all(|c| c.is_ascii_hexdigit());
    looks_like_commit.then(|| commit.to_string())
}

/// Reads lines from a child process stream, echoing each to the parent's
/// stdout/stderr (so build output keeps appearing in the supervisor log, as
/// it did with inherited stdio) while retaining a bounded tail of recent
/// lines for error reporting. Reads raw bytes rather than UTF-8 lines so a
/// stray invalid byte can't stop the pump mid-stream, and echoes through
/// tokio's async stdout/stderr so a stalled log sink cannot block runtime
/// worker threads.
fn pump_output<R>(
    reader: R,
    to_stderr: bool,
    tail: Arc<Mutex<VecDeque<String>>>,
) -> tokio::task::JoinHandle<()>
where
    R: AsyncRead + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut reader = BufReader::new(reader);
        let mut buf = Vec::new();
        let mut out = tokio::io::stdout();
        let mut err = tokio::io::stderr();
        let mut echo_ok = true;
        loop {
            buf.clear();
            match read_line_capped(&mut reader, &mut buf, COMMAND_OUTPUT_MAX_LINE_BYTES).await {
                Ok(true) => {}
                // EOF or read error: stop pumping
                Ok(false) | Err(_) => break,
            }

            // Echo with newline framing preserved even for truncated lines.
            if echo_ok {
                if !buf.ends_with(b"\n") {
                    buf.push(b'\n');
                }
                let res = if to_stderr {
                    err.write_all(&buf).await
                } else {
                    out.write_all(&buf).await
                };
                // If the parent's stdio is gone, keep draining the pipe so
                // the child doesn't block, but stop attempting to echo.
                echo_ok = res.is_ok();
            }

            let line = String::from_utf8_lossy(&buf);
            let mut line = line.trim_end_matches(['\r', '\n']).to_string();
            if line.len() > COMMAND_OUTPUT_MAX_LINE_LEN {
                let mut cut = COMMAND_OUTPUT_MAX_LINE_LEN;
                while !line.is_char_boundary(cut) {
                    cut -= 1;
                }
                line.truncate(cut);
                line.push('…');
            }
            let mut tail = tail.lock();
            if tail.len() == COMMAND_OUTPUT_TAIL_LINES {
                tail.pop_front();
            }
            tail.push_back(line);
        }
        if echo_ok {
            let _ = if to_stderr {
                err.flush().await
            } else {
                out.flush().await
            };
        }
    })
}

async fn run_command(cmd: &mut Command) -> Result<()> {
    cmd.stdout(Stdio::piped()).stderr(Stdio::piped());
    let mut child = cmd
        .spawn()
        .map_err(|e| anyhow!("Failed to spawn command: {cmd:?}: {e}"))?;

    let tail = Arc::new(Mutex::new(VecDeque::new()));
    let mut pumps = Vec::new();
    if let Some(stdout) = child.stdout.take() {
        pumps.push(pump_output(stdout, false, tail.clone()));
    }
    if let Some(stderr) = child.stderr.take() {
        pumps.push(pump_output(stderr, true, tail.clone()));
    }

    let status = child.wait().await?;

    // After the child exits the pumps drain to EOF almost immediately; the
    // timeout guards against a grandchild process holding the pipes open. On
    // timeout the pump is aborted so it can't linger and echo a grandchild's
    // output into the supervisor log long after the build step finished.
    for mut pump in pumps {
        if tokio::time::timeout(Duration::from_secs(5), &mut pump)
            .await
            .is_err()
        {
            pump.abort();
        }
    }

    if !status.success() {
        let tail = tail.lock();
        let output = if tail.is_empty() {
            "no output captured".to_string()
        } else {
            format!(
                "last output:\n{}",
                tail.iter()
                    .map(String::as_str)
                    .collect::<Vec<_>>()
                    .join("\n")
            )
        };
        return Err(anyhow!("Command failed ({status}): {cmd:?}; {output}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn test_version_from_resolved_binary() {
        let p = std::path::Path::new("/srv/llama.cpp/build-e37abd6b5/bin/llama-server");
        assert_eq!(
            version_from_resolved_binary(p).as_deref(),
            Some("e37abd6b5")
        );

        // Base name containing dashes: the commit is the suffix after the
        // last dash.
        let p = std::path::Path::new("/x/my-build-dir-cd5044661/bin/llama-server");
        assert_eq!(
            version_from_resolved_binary(p).as_deref(),
            Some("cd5044661")
        );

        // Non-hex or too-short suffixes are not commits.
        assert_eq!(
            version_from_resolved_binary(std::path::Path::new(
                "/opt/llama-builds/bin/llama-server"
            )),
            None
        );
        assert_eq!(
            version_from_resolved_binary(std::path::Path::new("/opt/build-v2/bin/llama-server")),
            None
        );

        // Hand-supplied binaries without the managed layout.
        assert_eq!(
            version_from_resolved_binary(std::path::Path::new("/usr/local/bin/llama-server")),
            None
        );
        assert_eq!(
            version_from_resolved_binary(std::path::Path::new("llama-server")),
            None
        );
    }

    #[tokio::test]
    async fn test_build_manager_flow() {
        // 1. Setup Temp Dir
        let temp_dir = std::env::temp_dir().join(format!(
            "llama-proxy-build-test-{}",
            ulid::Ulid::new().to_string()
        ));
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();

        // 2. Create a dummy "remote" repo
        let remote_repo = temp_dir.join("remote_repo");
        tokio::fs::create_dir_all(&remote_repo).await.unwrap();

        // Initialize git repo
        std::process::Command::new("git")
            .current_dir(&remote_repo)
            .arg("init")
            .output()
            .expect("Failed to init git");

        std::process::Command::new("git")
            .current_dir(&remote_repo)
            .args(["config", "user.email", "you@example.com"])
            .output()
            .expect("Failed to set user.email");

        std::process::Command::new("git")
            .current_dir(&remote_repo)
            .args(["config", "user.name", "Your Name"])
            .output()
            .expect("Failed to set user.name");

        tokio::fs::write(remote_repo.join("README.md"), "# Dummy Repo")
            .await
            .unwrap();

        std::process::Command::new("git")
            .current_dir(&remote_repo)
            .args(["add", "."])
            .output()
            .expect("Failed to git add");

        std::process::Command::new("git")
            .current_dir(&remote_repo)
            .args(["commit", "-m", "Initial commit"])
            .output()
            .expect("Failed to git commit");

        // 3. Create dummy cmake
        let cmake_path = temp_dir.join("cmake");
        let cmake_script = r#"#!/bin/sh
# Mock cmake
if [ "$1" = "--build" ]; then
    # Create the dummy binary
    # Arg 2 is usually the build dir
    BUILD_DIR="$2"
    mkdir -p "$BUILD_DIR/bin"
    echo '#!/bin/sh' > "$BUILD_DIR/bin/llama-server"
    echo 'exit 0' >> "$BUILD_DIR/bin/llama-server"
    chmod +x "$BUILD_DIR/bin/llama-server"
    echo "Mock build complete in $BUILD_DIR"
else
    echo "Mock cmake configuring"
fi
"#;
        tokio::fs::write(&cmake_path, cmake_script).await.unwrap();
        let mut perms = tokio::fs::metadata(&cmake_path)
            .await
            .unwrap()
            .permissions();
        perms.set_mode(0o755);
        tokio::fs::set_permissions(&cmake_path, perms)
            .await
            .unwrap();

        // 4. Prepare Config
        let repo_path = temp_dir.join("local_repo");
        let build_path = repo_path.join("build"); // BuildManager appends hash
        let binary_path = temp_dir.join("llama-server-link");

        let config = LlamaCppConfig {
            repo_url: remote_repo.to_string_lossy().to_string(),
            repo_path: repo_path.to_string_lossy().to_string(),
            build_path: build_path.to_string_lossy().to_string(),
            binary_path: binary_path.to_string_lossy().to_string(),
            branch: "master".to_string(),
            build_args: vec![],
            build_command_args: vec![],
            auto_update_interval_seconds: 0,
            enabled: true,
            keep_builds: 3,
        };

        // 5. Run BuildManager with modified PATH
        let original_path = std::env::var("PATH").unwrap_or_default();
        let new_path = format!("{}:{}", temp_dir.display(), original_path);
        unsafe {
            std::env::set_var("PATH", &new_path);
        }

        let bm = BuildManager::new(config);
        let result = bm.update_and_build().await;

        // Restore PATH
        unsafe {
            std::env::set_var("PATH", &original_path);
        }

        if let Err(e) = &result {
            println!("BuildManager Error: {e}");
        }
        assert!(result.is_ok());

        // Verify binary link exists
        assert!(binary_path.exists());
        assert!(tokio::fs::read_link(&binary_path).await.is_ok());

        // Verify build status was recorded
        let status = bm.build_status();
        assert!(!status.is_building);
        assert!(status.last_build_error.is_none());
        assert!(status.last_build_at.is_some());

        // 6. Re-run with the same commit: the verified binary is already
        // live, so no new swap may be signaled — a swap would needlessly
        // drain running instances. (This run takes the verify-existing fast
        // path, so the mock cmake is not needed again.)
        let gen_after_first = bm.binary_generation();
        assert!(gen_after_first >= 1);
        let result = bm.update_and_build().await;
        assert!(result.is_ok());
        assert_eq!(
            bm.binary_generation(),
            gen_after_first,
            "same-commit no-op rebuild must not signal a binary swap"
        );

        // 7. If the symlink points somewhere else, the same situation must
        // swap (and signal) again.
        tokio::fs::remove_file(&binary_path).await.unwrap();
        std::os::unix::fs::symlink(&cmake_path, &binary_path).unwrap();
        let result = bm.update_and_build().await;
        assert!(result.is_ok());
        assert_eq!(
            bm.binary_generation(),
            gen_after_first + 1,
            "stale symlink must be re-pointed and the swap signaled"
        );
        let resolved = tokio::fs::canonicalize(&binary_path).await.unwrap();
        assert!(resolved.ends_with("bin/llama-server"));

        // Cleanup
        let _ = tokio::fs::remove_dir_all(temp_dir).await;
    }

    #[tokio::test]
    async fn symlink_already_current_detects_live_and_stale_links() {
        let temp_dir = std::env::temp_dir().join(format!(
            "llamesh-symlink-test-{}",
            ulid::Ulid::new().to_string()
        ));
        tokio::fs::create_dir_all(&temp_dir).await.unwrap();
        let target = temp_dir.join("target-bin");
        tokio::fs::write(&target, "x").await.unwrap();
        let other = temp_dir.join("other-bin");
        tokio::fs::write(&other, "y").await.unwrap();
        let link = temp_dir.join("link");

        let config = crate::config::LlamaCppConfig {
            repo_url: "".into(),
            repo_path: "".into(),
            build_path: "".into(),
            binary_path: link.to_string_lossy().to_string(),
            branch: "".into(),
            build_args: vec![],
            build_command_args: vec![],
            auto_update_interval_seconds: 0,
            enabled: false,
            keep_builds: 3,
        };
        let bm = BuildManager::new(config);

        // No link exists yet.
        assert!(!bm.symlink_already_current(&target).await);

        std::os::unix::fs::symlink(&target, &link).unwrap();
        assert!(bm.symlink_already_current(&target).await);
        assert!(!bm.symlink_already_current(&other).await);

        // Dangling link (target removed) must not count as current.
        tokio::fs::remove_file(&target).await.unwrap();
        assert!(!bm.symlink_already_current(&target).await);

        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }

    #[test]
    fn test_is_building_initially_false() {
        let config = crate::config::LlamaCppConfig {
            repo_url: "".into(),
            repo_path: "".into(),
            build_path: "".into(),
            binary_path: "/nonexistent".into(),
            branch: "".into(),
            build_args: vec![],
            build_command_args: vec![],
            auto_update_interval_seconds: 0,
            enabled: false,
            keep_builds: 3,
        };
        let bm = BuildManager::new(config);
        assert!(!bm.is_building());
    }

    #[test]
    fn test_is_binary_available_nonexistent() {
        let config = crate::config::LlamaCppConfig {
            repo_url: "".into(),
            repo_path: "".into(),
            build_path: "".into(),
            binary_path: "/nonexistent/path/binary".into(),
            branch: "".into(),
            build_args: vec![],
            build_command_args: vec![],
            auto_update_interval_seconds: 0,
            enabled: false,
            keep_builds: 3,
        };
        let bm = BuildManager::new(config);
        assert!(!bm.is_binary_available());
    }

    #[test]
    fn test_can_serve_false_when_no_binary() {
        let config = crate::config::LlamaCppConfig {
            repo_url: "".into(),
            repo_path: "".into(),
            build_path: "".into(),
            binary_path: "/nonexistent/path/binary".into(),
            branch: "".into(),
            build_args: vec![],
            build_command_args: vec![],
            auto_update_interval_seconds: 0,
            enabled: false,
            keep_builds: 3,
        };
        let bm = BuildManager::new(config);
        assert!(!bm.can_serve());
    }

    #[tokio::test]
    async fn test_get_version_default() {
        let config = crate::config::LlamaCppConfig {
            repo_url: "".into(),
            repo_path: "".into(),
            build_path: "".into(),
            binary_path: "/nonexistent".into(),
            branch: "".into(),
            build_args: vec![],
            build_command_args: vec![],
            auto_update_interval_seconds: 0,
            enabled: false,
            keep_builds: 3,
        };
        let bm = BuildManager::new(config);
        let version = bm.get_version().await;
        assert_eq!(version, "unknown");
    }

    #[tokio::test]
    async fn test_get_version_reflects_only_recorded_running_version() {
        let config = crate::config::LlamaCppConfig {
            repo_url: "".into(),
            repo_path: "".into(),
            build_path: "".into(),
            binary_path: "/nonexistent".into(),
            branch: "".into(),
            build_args: vec![],
            build_command_args: vec![],
            auto_update_interval_seconds: 0,
            enabled: false,
            keep_builds: 3,
        };
        let bm = BuildManager::new(config);

        // Until a binary is actually swapped in, no commit is reported. This is
        // what keeps the metric/snapshot from advertising a commit that is only
        // checked out or still building.
        assert_eq!(bm.get_version().await, "unknown");

        // `record_running_version` is the sole setter and is only called after a
        // successful `update_symlink`, so this models the post-swap transition.
        bm.record_running_version("06d26dfdf");
        assert_eq!(bm.get_version().await, "06d26dfdf");

        // A subsequent successful swap moves the reported version forward.
        bm.record_running_version("deadbeef");
        assert_eq!(bm.get_version().await, "deadbeef");
    }

    #[test]
    fn test_invalidate_binary_cache() {
        let config = crate::config::LlamaCppConfig {
            repo_url: "".into(),
            repo_path: "".into(),
            build_path: "".into(),
            binary_path: "/nonexistent".into(),
            branch: "".into(),
            build_args: vec![],
            build_command_args: vec![],
            auto_update_interval_seconds: 0,
            enabled: false,
            keep_builds: 3,
        };
        let bm = BuildManager::new(config);
        // Just verify it doesn't panic
        bm.invalidate_binary_cache();
    }

    #[test]
    fn test_build_guard_clears_building_flag() {
        let building = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(true));
        {
            let _guard = BuildGuard {
                building: building.clone(),
            };
            assert!(building.load(std::sync::atomic::Ordering::Relaxed));
        }
        // After guard drops, building should be false
        assert!(!building.load(std::sync::atomic::Ordering::Relaxed));
    }

    #[test]
    fn test_build_status_initial() {
        let config = crate::config::LlamaCppConfig {
            repo_url: "".into(),
            repo_path: "".into(),
            build_path: "".into(),
            binary_path: "/nonexistent".into(),
            branch: "".into(),
            build_args: vec![],
            build_command_args: vec![],
            auto_update_interval_seconds: 0,
            enabled: false,
            keep_builds: 3,
        };
        let bm = BuildManager::new(config);
        let status = bm.build_status();
        assert!(!status.is_building);
        assert!(status.last_build_error.is_none());
        assert!(status.last_build_at.is_none());
    }

    #[test]
    fn test_can_serve_true_while_building() {
        let tmp = std::env::temp_dir().join("llamesh-test-can-serve-building");
        std::fs::write(&tmp, "fake").unwrap();

        let config = crate::config::LlamaCppConfig {
            repo_url: "".into(),
            repo_path: "".into(),
            build_path: "".into(),
            binary_path: tmp.to_string_lossy().to_string(),
            branch: "".into(),
            build_args: vec![],
            build_command_args: vec![],
            auto_update_interval_seconds: 0,
            enabled: false,
            keep_builds: 3,
        };
        let bm = BuildManager::new(config);

        // Binary exists and not building → can serve
        assert!(bm.can_serve());

        // Binary exists and building → should STILL serve
        bm.building
            .store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(
            bm.can_serve(),
            "Node must continue serving during builds when binary exists"
        );

        std::fs::remove_file(&tmp).unwrap();
    }

    #[test]
    fn test_record_build_success_clears_error() {
        let config = crate::config::LlamaCppConfig {
            repo_url: "".into(),
            repo_path: "".into(),
            build_path: "".into(),
            binary_path: "/nonexistent".into(),
            branch: "".into(),
            build_args: vec![],
            build_command_args: vec![],
            auto_update_interval_seconds: 0,
            enabled: false,
            keep_builds: 3,
        };
        let bm = BuildManager::new(config);

        // Simulate a failure first
        bm.record_build_failure("some error");
        let status = bm.build_status();
        assert_eq!(status.last_build_error.as_deref(), Some("some error"));
        assert!(status.last_build_at.is_some());

        // Now record success - error should be cleared
        bm.record_build_success();
        let status = bm.build_status();
        assert!(status.last_build_error.is_none());
        assert!(status.last_build_at.is_some());
    }

    #[tokio::test]
    async fn test_run_command_success() {
        run_command(&mut Command::new("true")).await.unwrap();
    }

    #[tokio::test]
    async fn test_run_command_failure_includes_status_and_output() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("echo configuring; echo 'CMake Error: CUDA Toolkit not found' >&2; exit 3");
        let err = run_command(&mut cmd).await.unwrap_err().to_string();
        assert!(err.contains("exit status: 3"), "missing status: {err}");
        assert!(
            err.contains("CMake Error: CUDA Toolkit not found"),
            "missing stderr line: {err}"
        );
        assert!(err.contains("configuring"), "missing stdout line: {err}");
    }

    #[tokio::test]
    async fn test_run_command_failure_without_output() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c").arg("exit 7");
        let err = run_command(&mut cmd).await.unwrap_err().to_string();
        assert!(err.contains("exit status: 7"), "missing status: {err}");
        assert!(err.contains("no output captured"), "{err}");
    }

    #[tokio::test]
    async fn test_run_command_failure_output_tail_is_bounded() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("i=1; while [ $i -le 200 ]; do echo line-$i; i=$((i+1)); done; exit 1");
        let err = run_command(&mut cmd).await.unwrap_err().to_string();
        // Only the last COMMAND_OUTPUT_TAIL_LINES (40) lines are kept: 161..=200
        assert!(err.contains("line-200"), "{err}");
        assert!(err.contains("line-161\n"), "{err}");
        assert!(!err.contains("line-160\n"), "{err}");
        assert!(!err.contains("line-1\n"), "{err}");
    }

    #[tokio::test]
    async fn test_run_command_failure_truncates_long_lines() {
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("printf 'x%.0s' $(seq 1 1000); echo; exit 1");
        let err = run_command(&mut cmd).await.unwrap_err().to_string();
        let long_line = err
            .lines()
            .find(|l| l.starts_with('x'))
            .expect("truncated output line present");
        assert!(
            long_line.chars().filter(|&c| c == 'x').count() == COMMAND_OUTPUT_MAX_LINE_LEN,
            "line not truncated to limit: {} chars",
            long_line.len()
        );
        assert!(long_line.ends_with('…'), "missing truncation marker");
    }

    #[tokio::test]
    async fn test_run_command_bounds_newline_free_output() {
        // A child emitting a huge line with no newline must not grow the
        // capture buffer without bound; the tail entry is still truncated.
        let mut cmd = Command::new("sh");
        cmd.arg("-c")
            .arg("head -c 1000000 /dev/zero | tr '\\0' 'y'; exit 1");
        let err = run_command(&mut cmd).await.unwrap_err().to_string();
        let long_line = err
            .lines()
            .find(|l| l.starts_with('y'))
            .expect("captured line present");
        assert!(
            long_line.chars().filter(|&c| c == 'y').count() == COMMAND_OUTPUT_MAX_LINE_LEN,
            "line not truncated to limit"
        );
        assert!(long_line.ends_with('…'), "missing truncation marker");
    }

    #[tokio::test]
    async fn test_read_line_capped() {
        let data: Vec<u8> = [b"short\n".to_vec(), vec![b'z'; 20000], b"\nnext\n".to_vec()].concat();
        let mut reader = BufReader::new(data.as_slice());
        let cap = COMMAND_OUTPUT_MAX_LINE_BYTES;

        let mut buf = Vec::new();
        assert!(read_line_capped(&mut reader, &mut buf, cap).await.unwrap());
        assert_eq!(buf, b"short\n");

        // Over-long line: capped at `cap` bytes, remainder discarded
        buf.clear();
        assert!(read_line_capped(&mut reader, &mut buf, cap).await.unwrap());
        assert_eq!(buf.len(), cap);
        assert!(buf.iter().all(|&b| b == b'z'));

        // The next line is read intact — the overflow didn't bleed into it
        buf.clear();
        assert!(read_line_capped(&mut reader, &mut buf, cap).await.unwrap());
        assert_eq!(buf, b"next\n");

        buf.clear();
        assert!(!read_line_capped(&mut reader, &mut buf, cap).await.unwrap());
    }

    #[tokio::test]
    async fn cleanup_preserves_active_build() {
        // Cleanup must never delete the build the live binary symlink resolves
        // to, even when keep_builds is 0 (which would otherwise remove every
        // build, including the one currently serving).
        let temp_dir =
            std::env::temp_dir().join(format!("llama-proxy-cleanup-test-{}", ulid::Ulid::new()));
        let llama = temp_dir.join("llama.cpp");

        // Three managed build dirs, each with a bin/llama-server file.
        for name in ["build-aaaaaaaaa", "build-bbbbbbbbb", "build-ccccccccc"] {
            let bin = llama.join(name).join("bin");
            tokio::fs::create_dir_all(&bin).await.unwrap();
            tokio::fs::write(bin.join("llama-server"), b"#!/bin/true\n")
                .await
                .unwrap();
        }

        // The live binary symlink (build/bin/llama-server) points at the middle
        // build, so the active build is neither the newest nor the oldest.
        let link_dir = llama.join("build").join("bin");
        tokio::fs::create_dir_all(&link_dir).await.unwrap();
        let link = link_dir.join("llama-server");
        let active = llama.join("build-bbbbbbbbb");
        std::os::unix::fs::symlink(active.join("bin").join("llama-server"), &link).unwrap();

        let config = crate::config::LlamaCppConfig {
            repo_url: String::new(),
            repo_path: llama.to_string_lossy().to_string(),
            build_path: llama.join("build").to_string_lossy().to_string(),
            binary_path: link.to_string_lossy().to_string(),
            branch: String::new(),
            build_args: vec![],
            build_command_args: vec![],
            auto_update_interval_seconds: 0,
            enabled: true,
            keep_builds: 0,
        };
        let bm = BuildManager::new(config);
        bm.cleanup_old_builds().await.unwrap();

        // The active build survives; the other two are removed.
        assert!(
            active.exists(),
            "active build dir must not be deleted by cleanup"
        );
        assert!(!llama.join("build-aaaaaaaaa").exists());
        assert!(!llama.join("build-ccccccccc").exists());

        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }

    #[tokio::test]
    async fn cleanup_keeps_configured_non_active_builds() {
        // Protecting the active build must not reduce how many previous builds
        // are kept: with keep_builds=2 and the active build being the oldest,
        // the two most recent non-active builds are retained alongside it, and
        // only the older non-active build is removed.
        let temp_dir = std::env::temp_dir().join(format!(
            "llama-proxy-cleanup-keep-test-{}",
            ulid::Ulid::new()
        ));
        let llama = temp_dir.join("llama.cpp");

        // Four build dirs created with ascending modification times so the sort
        // order is deterministic; the first created (active) is the oldest.
        let names = [
            "build-100000000",
            "build-200000000",
            "build-300000000",
            "build-400000000",
        ];
        for name in names {
            let bin = llama.join(name).join("bin");
            tokio::fs::create_dir_all(&bin).await.unwrap();
            tokio::fs::write(bin.join("llama-server"), b"#!/bin/true\n")
                .await
                .unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        }

        // The active build is the oldest (build-100000000).
        let link_dir = llama.join("build").join("bin");
        tokio::fs::create_dir_all(&link_dir).await.unwrap();
        let link = link_dir.join("llama-server");
        let active = llama.join("build-100000000");
        std::os::unix::fs::symlink(active.join("bin").join("llama-server"), &link).unwrap();

        let config = crate::config::LlamaCppConfig {
            repo_url: String::new(),
            repo_path: llama.to_string_lossy().to_string(),
            build_path: llama.join("build").to_string_lossy().to_string(),
            binary_path: link.to_string_lossy().to_string(),
            branch: String::new(),
            build_args: vec![],
            build_command_args: vec![],
            auto_update_interval_seconds: 0,
            enabled: true,
            keep_builds: 2,
        };
        BuildManager::new(config)
            .cleanup_old_builds()
            .await
            .unwrap();

        // Active kept, plus the two most recent non-active builds; the oldest
        // non-active build is removed.
        assert!(active.exists(), "active build must be kept");
        assert!(llama.join("build-300000000").exists());
        assert!(llama.join("build-400000000").exists());
        assert!(
            !llama.join("build-200000000").exists(),
            "the oldest non-active build should be removed"
        );

        let _ = tokio::fs::remove_dir_all(&temp_dir).await;
    }
}
