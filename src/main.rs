mod api;
mod build_manager;
mod circuit_breaker;
mod cluster;
mod config;
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

use clap::Parser;
use notify::{RecommendedWatcher, RecursiveMode, Watcher};
use std::path::PathBuf;
use std::sync::Arc;
use tracing::{error, info, warn};

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

        // Create a channel for filesystem events
        let (tx, mut rx) = tokio::sync::mpsc::channel::<notify::Result<notify::Event>>(16);

        // Spawn the watcher in a blocking thread since notify uses std channels
        let cookbook_path_for_watcher = cookbook_path.clone();
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
                    eprintln!("Failed to create cookbook file watcher: {}", e);
                    return;
                }
            };

            if let Err(e) = watcher.watch(&cookbook_path_for_watcher, RecursiveMode::NonRecursive) {
                eprintln!("Failed to watch cookbook file: {}", e);
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
                    path = %cookbook_path.display(),
                    "Started cookbook file watcher"
                );

                while let Some(event_result) = rx.recv().await {
                    match event_result {
                        Ok(event) => {
                            // Only react to modify/create events
                            if matches!(
                                event.kind,
                                notify::EventKind::Modify(_) | notify::EventKind::Create(_)
                            ) {
                                info!(
                                    event = "cookbook_change_detected",
                                    paths = ?event.paths,
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
    // If a binary already exists, requests are served using it until the build completes
    {
        let bm = node_state.build_manager.clone();
        let lock = node_state.rebuild_lock.clone();
        tokio::spawn(
            async move {
                let _permit = lock.lock().await;
                if let Err(e) = bm.update_and_build().await {
                    tracing::error!(event = "llama_build_failure", error = %e, "Initial llama.cpp build failed");
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
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(std::time::Duration::from_secs(interval_secs));
            // The first tick completes immediately, so we skip it to avoid double-build on startup
            interval.tick().await;

            loop {
                interval.tick().await;
                info!("Running scheduled llama.cpp update check");

                // Try to acquire lock
                if let Ok(_permit) = lock.try_lock() {
                    if let Err(e) = bm.update_and_build().await {
                        tracing::error!(event = "llama_build_failure", error = %e, "Scheduled update failed");
                    }
                } else {
                    tracing::warn!("Skipping scheduled update because a rebuild is already in progress");
                }
            }
        }.instrument(span.clone()));
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
