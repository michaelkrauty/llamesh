mod api;
mod build_manager;
mod circuit_breaker;
mod cluster;
mod config;
mod connection;
#[allow(clippy::items_after_test_module)]
mod discovery;
mod errors;
mod health;
mod instance;
mod logging;
mod memory_sampler;
mod metrics;
mod node_state;
mod noise;
mod protocol_detect;
mod router;
mod security;
mod util;
mod wedge_detector;

use clap::Parser;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use tracing::{error, info, warn};

/// Backoff schedule for the initial llama.cpp build. Covers roughly the
/// first twenty minutes after startup, long enough for boot-time DNS and
/// network bring-up; after that the scheduled update check takes over.
const INITIAL_BUILD_RETRY_DELAYS: &[Duration] = &[
    Duration::from_secs(10),
    Duration::from_secs(30),
    Duration::from_secs(60),
    Duration::from_secs(120),
    Duration::from_secs(300),
    Duration::from_secs(600),
];

/// Backoff schedule for a scheduled (periodic) llama.cpp update check. Unlike
/// the initial build, this runs while the node is already serving with the
/// network normally up, so a failure is usually a brief `git fetch` blip;
/// retrying within the cycle smooths it over instead of skipping a whole
/// `auto_update_interval_seconds` (commonly a day). Kept short and bounded so a
/// persistently failing upstream build is reattempted only a few times before
/// the next scheduled tick.
const SCHEDULED_UPDATE_RETRY_DELAYS: &[Duration] = &[
    Duration::from_secs(30),
    Duration::from_secs(120),
    Duration::from_secs(300),
];

// An empty schedule would silently make a single `git fetch` blip skip a whole
// `auto_update_interval_seconds` (commonly a day) and log it at ERROR as a build
// failure. Enforce a non-empty schedule at compile time.
const _: () = assert!(!SCHEDULED_UPDATE_RETRY_DELAYS.is_empty());

#[derive(Parser, Debug)]
#[command(author, version, about, long_about = None)]
struct Args {
    /// Path to config file
    #[arg(long, default_value = "config.yaml")]
    config: PathBuf,

    /// Path to cookbook file
    #[arg(long, default_value = "cookbook.yaml")]
    cookbook: PathBuf,

    /// Path to secrets file (optional)
    #[arg(long)]
    secrets: Option<PathBuf>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let args = Args::parse();

    // Load configuration (before logging init so we can configure file logging)
    let mut config = match config::load_config(&args.config) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to load config: {e}");
            std::process::exit(1);
        }
    };
    let cookbook = match config::load_cookbook(&args.cookbook) {
        Ok(c) => c,
        Err(e) => {
            eprintln!("Failed to load cookbook: {e}");
            std::process::exit(1);
        }
    };
    if let Err(e) = cookbook.validate() {
        eprintln!("Invalid cookbook: {e}");
        std::process::exit(1);
    }

    if let Some(secrets_path) = &args.secrets {
        let secrets = config::load_secrets(secrets_path)?;
        config.merge_secrets(secrets);
    } else {
        // Try loading secrets.yaml from default location if it exists
        let default_secrets = std::path::Path::new("secrets.yaml");
        if default_secrets.exists() {
            if let Ok(secrets) = config::load_secrets(default_secrets) {
                config.merge_secrets(secrets);
            }
        }
    }

    // Validate config
    if let Err(e) = config.validate() {
        eprintln!("Invalid config: {e}");
        std::process::exit(1);
    }

    // Initialize logging (after config load for file logging support)
    logging::init(config.logging.as_ref())?;

    info!("Starting llamesh");
    info!("Version: {}", env!("CARGO_PKG_VERSION"));
    info!("Loaded config for node: {}", config.node_id);
    info!("Loaded cookbook with {} models", cookbook.models.len());
    if let Some(secrets_path) = &args.secrets {
        info!("Loaded secrets from {:?}", secrets_path);
    }

    // Ensure metrics directory exists
    let metrics_path = std::path::Path::new(&config.metrics_path);
    if let Some(parent) = metrics_path.parent() {
        if !parent.exists() {
            std::fs::create_dir_all(parent)?;
            info!("Created metrics directory: {:?}", parent);
        }
    }

    // Create build manager (build happens in background after server starts)
    let build_manager = build_manager::BuildManager::new(config.llama_cpp.clone());

    // Initialize node state
    let node_state =
        node_state::NodeState::new(config.clone(), cookbook.clone(), build_manager).await?;
    let shutdown_state = node_state.clone();

    // Start background tasks
    let bg_state = Arc::new(node_state.clone());
    let gossip_state = Arc::new(node_state.clone());

    let span = tracing::info_span!("node", node_id = %config.node_id);

    use tracing::Instrument;

    tokio::spawn(
        async move {
            bg_state.run_background_tasks().await;
        }
        .instrument(span.clone()),
    );

    tokio::spawn(
        async move {
            cluster::start_gossip_loop(gossip_state).await;
        }
        .instrument(span.clone()),
    );

    // Start cookbook file watcher for hot reloading
    // Shutdown channel for clean watcher thread termination
    let (watcher_shutdown_tx, watcher_shutdown_rx) = std::sync::mpsc::channel::<()>();
    {
        let cookbook_path = args.cookbook.clone();
        let watcher_state = node_state.clone();
        let watcher_span = span.clone();

        // Resolve the directory that contains the cookbook. We watch the
        // directory — not the file — so edits that replace the inode (atomic
        // rename used by `sed -i`, most editors' save-atomic, IDE autosave)
        // keep producing events. A single-file watch breaks after any such
        // edit because inotify's watch is bound to the original inode and is
        // silently lost when that inode is unlinked.
        let canonical_cookbook_path = match std::fs::canonicalize(&cookbook_path) {
            Ok(p) => p,
            Err(e) => {
                eprintln!(
                    "Failed to canonicalize cookbook path {}: {}",
                    cookbook_path.display(),
                    e
                );
                return Err(e.into());
            }
        };
        let cookbook_dir = canonical_cookbook_path
            .parent()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Cookbook path has no parent directory: {}",
                    canonical_cookbook_path.display()
                )
            })?
            .to_path_buf();
        let cookbook_filename = canonical_cookbook_path
            .file_name()
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Cookbook path has no file name: {}",
                    canonical_cookbook_path.display()
                )
            })?
            .to_os_string();

        // Create a channel for filesystem events
        let (tx, mut rx) = tokio::sync::mpsc::channel::<notify::Result<notify::Event>>(16);

        // Spawn the watcher in a blocking thread since notify uses std channels
        let watched_dir = cookbook_dir.clone();
        let rt = tokio::runtime::Handle::current();
        std::thread::spawn(move || {
            let rt = rt.clone();
            let tx = tx;

            let mut watcher = match RecommendedWatcher::new(
                move |res| {
                    let _ = rt.block_on(async { tx.send(res).await });
                },
                notify::Config::default(),
            ) {
                Ok(w) => w,
                Err(e) => {
                    eprintln!("Failed to create cookbook file watcher: {e}");
                    return;
                }
            };

            if let Err(e) = watcher.watch(&watched_dir, RecursiveMode::NonRecursive) {
                eprintln!("Failed to watch cookbook directory: {e}");
                return;
            }

            // Keep the watcher alive until shutdown signal received
            // Using recv() which blocks until message or channel disconnection
            let _ = watcher_shutdown_rx.recv();
            // Watcher is dropped here, cleaning up resources
        });

        // Handle events in async context
        tokio::spawn(
            async move {
                info!(
                    path = %canonical_cookbook_path.display(),
                    watched_dir = %cookbook_dir.display(),
                    "Started cookbook file watcher"
                );

                while let Some(event_result) = rx.recv().await {
                    match event_result {
                        Ok(event) => {
                            // Only react to events that touch the cookbook file.
                            // notify emits absolute paths when watching an
                            // absolute directory path, so filename comparison
                            // against `cookbook_filename` is sufficient.
                            let touches_cookbook = event
                                .paths
                                .iter()
                                .any(|p| p.file_name() == Some(&cookbook_filename));
                            if !touches_cookbook {
                                continue;
                            }

                            // Ignore metadata-only changes (chmod, touch) —
                            // the file's contents weren't rewritten, so the
                            // reload would be a no-op at best and a thrash at
                            // worst (many editors emit a burst of events).
                            let kind_is_reloadable = matches!(
                                event.kind,
                                notify::EventKind::Modify(notify::event::ModifyKind::Data(_))
                                    | notify::EventKind::Modify(notify::event::ModifyKind::Name(_))
                                    | notify::EventKind::Modify(notify::event::ModifyKind::Any)
                                    | notify::EventKind::Create(_)
                                    | notify::EventKind::Remove(_)
                            );
                            if !kind_is_reloadable {
                                continue;
                            }

                            info!(
                                event = "cookbook_change_detected",
                                paths = ?event.paths,
                                kind = ?event.kind,
                                "Cookbook file changed, reloading"
                            );

                            // Small debounce delay to handle rapid successive events
                            tokio::time::sleep(std::time::Duration::from_millis(100)).await;

                            match watcher_state.reload_cookbook(&cookbook_path).await {
                                Ok(()) => {
                                    // Successfully reloaded - the reload_cookbook method logs this
                                }
                                Err(e) => {
                                    error!(
                                        event = "cookbook_reload_failed",
                                        error = %e,
                                        "Failed to reload cookbook"
                                    );
                                }
                            }
                        }
                        Err(e) => {
                            warn!(
                                error = %e,
                                "Cookbook file watcher error"
                            );
                        }
                    }
                }
            }
            .instrument(watcher_span),
        );
    }

    // Initial build - runs in background so server can start immediately
    // If a binary already exists, requests are served using it until the build completes.
    //
    // Failures here are retried with backoff: at boot the network/DNS is often
    // not up yet when the service starts, and without retries a transient
    // `git fetch` failure would leave the node reporting an unknown llama.cpp
    // version until the next scheduled update check (commonly a day away).
    {
        let bm = node_state.build_manager.clone();
        let lock = node_state.rebuild_lock.clone();
        tokio::spawn(
            async move {
                // Every successful build — initial, scheduled, or manual —
                // bumps the binary generation when the symlink is swapped.
                // A generation above the one observed here is a durable
                // "another path already succeeded" marker for cancelling the
                // retry loop: unlike the recorded build error/timestamp, it
                // cannot be reset by a rebuild that fails afterwards. Once
                // any build has succeeded this loop's goal is met; later
                // failures are the scheduled update check's concern, and
                // redundantly re-running the build here would re-signal a
                // binary swap for the same commit and needlessly drain
                // freshly spawned instances.
                let startup_generation = bm.binary_generation();
                let outcome = util::retry_with_backoff(
                    "initial llama.cpp build",
                    INITIAL_BUILD_RETRY_DELAYS,
                    // Stop retrying if another path (e.g. the manual rebuild
                    // endpoint) completed a successful build while this loop
                    // was sleeping.
                    || bm.binary_generation() == startup_generation,
                    // The rebuild lock is held only while an attempt runs —
                    // never across backoff sleeps — so the manual rebuild
                    // endpoint stays usable between attempts.
                    || {
                        let bm = bm.clone();
                        let lock = lock.clone();
                        async move {
                            let _permit = lock.lock().await;
                            // Re-check after acquiring the lock: a concurrent
                            // rebuild may have succeeded while this attempt
                            // waited for it.
                            if bm.binary_generation() > startup_generation {
                                tracing::debug!(
                                    "Skipping initial build attempt; a concurrent rebuild already succeeded"
                                );
                                return Ok(());
                            }
                            bm.update_and_build().await
                        }
                    },
                )
                .await;
                match outcome {
                    util::RetryOutcome::Success(()) => {}
                    util::RetryOutcome::Aborted(e) => {
                        tracing::debug!(
                            error = %e,
                            "Initial llama.cpp build retries stopped; a concurrent rebuild already succeeded"
                        );
                    }
                    util::RetryOutcome::Exhausted(e) => {
                        tracing::error!(
                            event = "llama_build_failure",
                            error = %e,
                            "Initial llama.cpp build failed after all retries; next scheduled update check will try again"
                        );
                    }
                }
            }
            .instrument(span.clone()),
        );
    }

    // Auto-update loop
    if config.llama_cpp.auto_update_interval_seconds > 0 {
        let bm = node_state.build_manager.clone();
        let lock = node_state.rebuild_lock.clone();
        let interval_secs = config.llama_cpp.auto_update_interval_seconds;
        tokio::spawn(
            async move {
                let mut interval =
                    tokio::time::interval(std::time::Duration::from_secs(interval_secs));
                // The first tick completes immediately, so we skip it to avoid double-build on startup
                interval.tick().await;

                loop {
                    interval.tick().await;
                    info!("Running scheduled llama.cpp update check");

                    // Defer to a build already running (manual rebuild, or a
                    // still-running initial build): skip this cycle rather than
                    // queueing behind it. The probe permit is released immediately;
                    // the retry loop below re-acquires the lock per attempt.
                    if lock.try_lock().is_err() {
                        tracing::warn!(
                            "Skipping scheduled update because a rebuild is already in progress"
                        );
                        continue;
                    }

                    // Retry transient failures (e.g. a momentary DNS/network blip
                    // on `git fetch`) within this cycle instead of skipping a whole
                    // auto_update_interval_seconds — commonly a day. The lock is
                    // re-acquired per attempt and never held across a backoff
                    // sleep, so the manual rebuild endpoint stays responsive
                    // between attempts. Only an exhausted retry budget escalates to
                    // an ERROR-level build-failure event; a transient blip that
                    // recovers is logged at WARN by the retry helper and clears.
                    let outcome = util::retry_with_backoff(
                        "scheduled llama.cpp update",
                        SCHEDULED_UPDATE_RETRY_DELAYS,
                        || true,
                        || {
                            let bm = bm.clone();
                            let lock = lock.clone();
                            async move {
                                let _permit = lock.lock().await;
                                bm.update_and_build().await
                            }
                        },
                    )
                    .await;

                    match outcome {
                        util::RetryOutcome::Success(()) | util::RetryOutcome::Aborted(_) => {}
                        util::RetryOutcome::Exhausted(e) => {
                            tracing::error!(
                                event = "llama_build_failure",
                                error = %e,
                                "Scheduled update failed after retries"
                            );
                        }
                    }
                }
            }
            .instrument(span.clone()),
        );
    }

    // Binary swap watcher — drains running instances when build manager swaps the binary
    {
        let drain_state = node_state.clone();
        let swap_notify = drain_state.build_manager.binary_swap_notify().clone();
        tokio::spawn(
            async move {
                // Track the generation we've already processed so we don't drain on startup
                let mut last_gen = drain_state.build_manager.binary_generation();
                loop {
                    swap_notify.notified().await;
                    let current_gen = drain_state.build_manager.binary_generation();
                    if current_gen > last_gen {
                        info!(
                            event = "binary_swap_detected",
                            old_generation = last_gen,
                            new_generation = current_gen,
                            "Binary swap detected, draining existing instances"
                        );
                        drain_state.drain_instances_for_binary_update().await;
                        last_gen = current_gen;
                    }
                }
            }
            .instrument(span.clone()),
        );
    }

    // Wedged-instance watchdog — stops a local instance that holds a request
    // slot while flat at ~0% CPU and ~0% GPU (a hung upstream), releasing the
    // stuck slot. Opt-in; not spawned at all when disabled (zero overhead).
    if node_state.config.wedge_detector.enabled {
        let detector_state = node_state.clone();
        let interval_ms = node_state.config.wedge_detector.sample_interval_ms;
        info!(
            window_ms = node_state.config.wedge_detector.window_ms,
            sample_interval_ms = interval_ms,
            "Wedged-instance watchdog enabled"
        );
        tokio::spawn(
            async move {
                let mut ticker = tokio::time::interval(Duration::from_millis(interval_ms));
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    ticker.tick().await;
                    detector_state.detect_and_kill_wedged_instances().await;
                }
            }
            .instrument(span.clone()),
        );
    }

    // Start API server
    api::start_server(config, node_state)
        .instrument(span.clone())
        .await?;

    // Signal watcher thread to shutdown
    drop(watcher_shutdown_tx);

    // Graceful shutdown of instances
    shutdown_state.shutdown_all_instances().await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::Arc;

    // The non-empty-schedule invariant this test used to assert at runtime is
    // now enforced at compile time next to `SCHEDULED_UPDATE_RETRY_DELAYS`.

    // A transient failure recovers within the scheduled retry budget without
    // reaching the exhausted (ERROR-logged) outcome — the behavior this
    // schedule exists to provide. Paused time auto-advances the backoff sleeps.
    #[tokio::test(start_paused = true)]
    async fn scheduled_update_recovers_from_transient_failure() {
        let calls = Arc::new(AtomicUsize::new(0));
        let calls_op = calls.clone();
        let outcome = util::retry_with_backoff(
            "scheduled llama.cpp update",
            SCHEDULED_UPDATE_RETRY_DELAYS,
            || true,
            || {
                let calls = calls_op.clone();
                async move {
                    let n = calls.fetch_add(1, Ordering::SeqCst) + 1;
                    if n == 1 {
                        Err(anyhow::anyhow!("transient git fetch failure"))
                    } else {
                        Ok(())
                    }
                }
            },
        )
        .await;
        assert!(matches!(outcome, util::RetryOutcome::Success(())));
        assert_eq!(calls.load(Ordering::SeqCst), 2);
    }
}
