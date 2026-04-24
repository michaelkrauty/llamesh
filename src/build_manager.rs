use crate::config::LlamaCppConfig;
use anyhow::{anyhow, Result};
use parking_lot::RwLock;
use serde::Serialize;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
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

        {
            let mut lock = self.current_version.write();
            *lock = Some(commit_hash.clone());
        }

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
                info!("Existing binary verified. Switching symlink.");
                self.update_symlink(&actual_binary_path).await?;
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

        // Keep recent builds based on config
        let keep_count = self.config.keep_builds;
        if builds.len() > keep_count {
            let to_remove = builds.len() - keep_count;
            for (path, _) in builds.iter().take(to_remove) {
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

async fn run_command(cmd: &mut Command) -> Result<()> {
    let status = cmd.status().await?;
    if !status.success() {
        return Err(anyhow!("Command failed: {cmd:?}"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

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

        // Cleanup
        let _ = tokio::fs::remove_dir_all(temp_dir).await;
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
}
