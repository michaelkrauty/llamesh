mod model_index;
mod peer_state;
mod port_pool;

pub use model_index::{build_pre_args, get_args_hash_for_key, ModelIndex};
pub use peer_state::{PeerModelStats, PeerState};
pub use port_pool::PortPool;

use crate::build_manager::BuildManager;
use crate::config::{Cookbook, ModelDefaults, NodeConfig, Profile};
use crate::discovery::Discovery;
use crate::instance::{compute_args_hash, Instance, InstanceStatus};
use crate::memory_sampler::MemorySampler;
use crate::metrics::Metrics;
use crate::noise::NoiseContext;
use anyhow::Result;
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{oneshot, Notify, RwLock};
use tracing::{error, info, warn};
use ulid::Ulid;

#[derive(Debug, thiserror::Error)]
pub enum NodeError {
    #[error("Max instances for this profile reached")]
    MaxInstancesProfile,
    #[error("Max instances per node reached")]
    MaxInstancesNode,
    #[error("Queue full")]
    QueueFull,
    #[error("Queue timeout")]
    QueueTimeout,
    #[error("Queue error")]
    QueueError,
    #[error("Insufficient resources even after eviction")]
    InsufficientResources,
    #[error("Yield to pending waiters")]
    YieldToWaiters,
    #[error("Local spawn failures exhausted, try peer")]
    SpawnFailuresExhausted,
    #[error("No available ports")]
    NoAvailablePorts,
    #[error("Port {0} already in use")]
    PortInUse(u16),
    #[error("Missing value for --host in profile {0}")]
    MissingHostArg(String),
    #[error("Missing value for --port in profile {0}")]
    MissingPortArg(String),
    #[error("Invalid port '{0}' in profile {1}")]
    InvalidPortArg(String, String),
    #[error(transparent)]
    Other(#[from] anyhow::Error),
}

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// NodeState holds all runtime state for a mesh proxy node.
///
/// # Lock Ordering (CRITICAL - READ BEFORE MODIFYING)
///
/// To prevent deadlocks, locks MUST be acquired in the following order:
///
/// ```text
/// 1. instances (read or write)
///    └─► 2. queues (read or write)
///        └─► 3. pending_tokens (read or write)
///            └─► 4. Individual Instance locks (via instances.get())
/// ```
///
/// ## Key Patterns
///
/// - **notify_queue()**: Drops `queues` lock BEFORE acquiring `pending_tokens`.
///   Token is generated atomically before unlock, ensuring consistency.
///
/// - **try_get_or_spawn()**: Acquires `instances` write lock, may call
///   `notify_queue()` which needs `queues` → must release `instances` first.
///
/// - **eviction paths**: Identify victims under read lock, then re-verify
///   and remove under write lock to avoid TOCTOU races.
///
/// ## Rules
///
/// 1. Never hold multiple top-level locks simultaneously if avoidable
/// 2. If you must hold multiple locks, follow the order above STRICTLY
/// 3. Always release higher-numbered locks before acquiring lower-numbered
/// 4. Use `drop(guard)` explicitly when releasing mid-function for clarity
/// 5. Add `// LOCK_ORDER: ...` comments when acquiring locks in complex paths
#[derive(Clone)]
pub struct NodeState {
    pub config: Arc<NodeConfig>,
    pub cookbook: Arc<RwLock<Cookbook>>,
    /// Version counter incremented on each cookbook reload.
    /// Callers can capture this at request start and compare at end
    /// to detect if cookbook changed mid-request.
    pub cookbook_version: Arc<AtomicU64>,
    pub instances: Arc<RwLock<HashMap<String, Arc<RwLock<Instance>>>>>,
    pub port_pool: Arc<PortPool>,
    pub http_client: reqwest::Client,
    pub cluster_client: reqwest::Client,
    /// Noise context for accepting connections (when enabled)
    pub noise_context: Option<Arc<NoiseContext>>,
    pub metrics: Arc<Metrics>,
    pub peers: Arc<RwLock<HashMap<String, PeerState>>>,
    pub queues: Arc<RwLock<HashMap<String, VecDeque<oneshot::Sender<u64>>>>>,
    /// Pending tokens for waiters who have been notified but haven't acquired a slot yet.
    /// Prevents fresh requests from bypassing queued waiters during the notification-to-acquisition window.
    pub pending_tokens: Arc<RwLock<HashMap<String, HashSet<u64>>>>,
    /// Counter for generating unique pending tokens
    pending_token_counter: Arc<AtomicU64>,
    pub draining: Arc<AtomicBool>,
    pub build_manager: Arc<BuildManager>,
    pub rebuild_lock: Arc<tokio::sync::Mutex<()>>,
    pub shutdown_notify: Arc<Notify>,
    /// Index for O(1) model lookups
    model_index: Arc<ModelIndex>,
    /// Memory sampler for VRAM and system memory measurement.
    pub memory_sampler: Arc<MemorySampler>,
    /// mDNS peer discovery (if enabled)
    pub discovery: Option<Arc<Discovery>>,
    /// Circuit breaker for peer connections
    pub circuit_breaker: Arc<crate::circuit_breaker::CircuitBreaker>,
    /// Notified when cluster capacity changes (instance idle, terminated, peer update).
    /// Used by route_or_wait() to efficiently wait for capacity.
    pub capacity_notify: Arc<Notify>,
    /// Model:profile keys that failed to spawn due to InsufficientResources
    /// and need eviction of existing instances to proceed. Used by the drain
    /// scheduler to decide when to drain an incumbent model.
    pub needs_eviction: Arc<RwLock<HashSet<String>>>,
    /// Signalled to trigger an immediate gossip round to peers (e.g., after
    /// an instance stops, starts, or is drained). Complements the periodic
    /// gossip interval for latency-sensitive state changes.
    pub gossip_trigger: Arc<Notify>,
}

use crate::security;

impl NodeState {
    pub async fn new(
        config: NodeConfig,
        cookbook: Cookbook,
        build_manager: BuildManager,
    ) -> Result<Self> {
        let cluster_client = security::build_cluster_client(&config.cluster_tls)?;
        let http_client = reqwest::Client::builder().build()?;

        let metrics = Metrics::load(std::path::Path::new(&config.metrics_path)).await;
        let model_index = Arc::new(ModelIndex::new(&cookbook));

        // Initialize circuit breaker (before moving config into Arc)
        let circuit_breaker = Arc::new(crate::circuit_breaker::CircuitBreaker::new(
            config.cluster.circuit_breaker.clone(),
        ));

        // Initialize noise protocol if enabled (server-side only)
        let noise_context = if config.cluster.enabled && config.cluster.noise.enabled {
            match crate::noise::initialize(&config.cluster.noise).await {
                Ok(ctx) => {
                    info!(
                        pubkey = %ctx.public_key_display(),
                        "Initialized Noise Protocol for cluster communication"
                    );
                    Some(Arc::new(ctx))
                }
                Err(e) => {
                    error!(error = %e, "Failed to initialize Noise Protocol, falling back to mTLS");
                    None
                }
            }
        } else {
            None
        };

        // Initialize mDNS discovery if enabled
        let discovery = if config.cluster.enabled && config.cluster.discovery.mdns {
            // Parse listen port from addr string (e.g., "0.0.0.0:8080")
            let listen_port = config
                .listen_addr
                .rsplit(':')
                .next()
                .and_then(|p| p.parse::<u16>().ok())
                .unwrap_or(8080);

            let public_key = noise_context
                .as_ref()
                .map(|ctx| ctx.public_key_display())
                .unwrap_or_default();

            match Discovery::new(&config.cluster, &config.node_id, listen_port, &public_key) {
                Ok(disc) => {
                    info!(
                        service_name = %config.cluster.discovery.service_name,
                        "mDNS discovery enabled"
                    );
                    Some(Arc::new(disc))
                }
                Err(e) => {
                    warn!(error = %e, "Failed to initialize mDNS discovery, continuing without it");
                    None
                }
            }
        } else {
            None
        };

        // Initialize port pool
        let port_pool = Arc::new(PortPool::new(config.llama_cpp_ports.clone()));

        Ok(Self {
            config: Arc::new(config),
            cookbook: Arc::new(RwLock::new(cookbook)),
            cookbook_version: Arc::new(AtomicU64::new(0)),
            instances: Arc::new(RwLock::new(HashMap::new())),
            port_pool,
            http_client,
            cluster_client,
            noise_context,
            metrics: Arc::new(metrics),
            peers: Arc::new(RwLock::new(HashMap::new())),
            queues: Arc::new(RwLock::new(HashMap::new())),
            pending_tokens: Arc::new(RwLock::new(HashMap::new())),
            pending_token_counter: Arc::new(AtomicU64::new(0)),
            draining: Arc::new(AtomicBool::new(false)),
            build_manager: Arc::new(build_manager),
            rebuild_lock: Arc::new(tokio::sync::Mutex::new(())),
            shutdown_notify: Arc::new(Notify::new()),
            model_index,
            memory_sampler: Arc::new(MemorySampler::new()),
            discovery,
            circuit_breaker,
            capacity_notify: Arc::new(Notify::new()),
            needs_eviction: Arc::new(RwLock::new(HashSet::new())),
            gossip_trigger: Arc::new(Notify::new()),
        })
    }

    /// Reload the cookbook from the given path. Returns Ok(()) if successful.
    /// Increments cookbook_version to allow detection of mid-request changes.
    pub async fn reload_cookbook(&self, path: &std::path::Path) -> Result<()> {
        let new_cookbook = crate::config::load_cookbook(path)?;
        new_cookbook.validate()?;

        let model_count = new_cookbook.models.len();

        // Update model index
        self.model_index.rebuild(&new_cookbook).await;

        // Update cookbook
        let mut cookbook = self.cookbook.write().await;
        *cookbook = new_cookbook;
        drop(cookbook);

        let new_version = self.cookbook_version.fetch_add(1, Ordering::SeqCst) + 1;

        info!(
            event = "cookbook_reload",
            model_count = model_count,
            version = new_version,
            path = %path.display(),
            "Cookbook reloaded successfully"
        );

        // Wake all queued waiters so they re-attempt with the updated cookbook.
        // A config change (e.g. reduced context length) may lower VRAM estimates
        // enough to unblock previously-stuck spawns.
        self.notify_all_queues().await;

        // Trigger instant gossip so peers learn about model changes immediately
        self.gossip_trigger.notify_one();

        Ok(())
    }

    pub async fn get_available_port(&self) -> Result<u16, NodeError> {
        self.port_pool
            .get_available_port()
            .await
            .map_err(|e| match e {
                port_pool::PortError::NoAvailablePorts => NodeError::NoAvailablePorts,
                port_pool::PortError::PortInUse(p) => NodeError::PortInUse(p),
            })
    }

    pub async fn release_port(&self, port: u16) {
        self.port_pool.release_port(port).await;
    }

    pub async fn reserve_specific_port(&self, port: u16) -> Result<(), NodeError> {
        self.port_pool
            .reserve_specific_port(port)
            .await
            .map_err(|e| match e {
                port_pool::PortError::NoAvailablePorts => NodeError::NoAvailablePorts,
                port_pool::PortError::PortInUse(p) => NodeError::PortInUse(p),
            })
    }

    pub async fn resolve_model(&self, model_name: &str) -> Option<(String, Profile)> {
        self.model_index.resolve(model_name).await
    }

    /// Find a peer that supports a model (case-insensitive).
    /// Used when local cookbook doesn't have the model but a peer might.
    /// Returns the PeerState of the best candidate peer, if any.
    pub async fn find_peer_for_model(&self, model_name: &str) -> Option<PeerState> {
        let peers = self.peers.read().await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        let timeout = self.config.cluster.gossip_interval_seconds.saturating_mul(3).max(15);

        // Parse model:profile from request
        let parts: Vec<&str> = model_name.split(':').collect();
        let base_model = parts[0];
        let profile_id = if parts.len() > 1 { parts[1] } else { "default" };

        let mut candidates: Vec<&PeerState> = Vec::new();

        for peer in peers.values() {
            // Skip stale peers
            if now.saturating_sub(peer.last_seen) > timeout {
                continue;
            }
            // Skip peers that aren't ready
            if !peer.ready {
                continue;
            }
            // Skip peers with open circuit breaker
            if !self.circuit_breaker.should_allow_sync(&peer.node_id) {
                continue;
            }
            // Check if peer supports this model (case-insensitive)
            if peer_supports_profile(peer, base_model, profile_id) {
                candidates.push(peer);
            }
        }

        if candidates.is_empty() {
            info!(
                event = "no_peer_for_model",
                model = %model_name,
                "No peer found that supports this model"
            );
            return None;
        }

        // Pick best candidate: prefer one with model already loaded, then lowest queue
        candidates.sort_by(|a, b| {
            let requested_key = format!("{}:{}", base_model, profile_id).to_lowercase();
            let loaded_a = a
                .loaded_models
                .iter()
                .any(|m| m.to_lowercase() == requested_key);
            let loaded_b = b
                .loaded_models
                .iter()
                .any(|m| m.to_lowercase() == requested_key);

            // Prefer loaded model
            match (loaded_a, loaded_b) {
                (true, false) => std::cmp::Ordering::Less,
                (false, true) => std::cmp::Ordering::Greater,
                _ => {
                    // Then by queue length
                    a.total_queue_length.cmp(&b.total_queue_length)
                }
            }
        });

        let chosen = candidates.first().cloned().cloned();
        if let Some(ref peer) = chosen {
            info!(
                event = "peer_found_for_model",
                model = %model_name,
                peer = %peer.node_id,
                "Found peer that supports model not in local cookbook"
            );
        }
        chosen
    }

    /// Get parsed model params for a model:profile key from any ready instance.
    /// Returns None if no ready instance exists or params haven't been parsed yet.
    pub async fn get_parsed_params_for_model(
        &self,
        model_name: &str,
        profile_id: &str,
    ) -> Option<crate::instance::ParsedModelParams> {
        let instances = self.instances.read().await;
        for inst_lock in instances.values() {
            let inst = inst_lock.read().await;
            if inst.model_name == model_name
                && inst.profile_id == profile_id
                && *inst.status.lock() == crate::instance::InstanceStatus::Ready
            {
                if let Some(params) = inst.get_parsed_params() {
                    return Some(params);
                }
            }
        }
        None
    }

    pub async fn calculate_current_load(&self) -> (u64, u64) {
        let instances = self.instances.read().await;
        let mut vram = 0;
        let mut sysmem = 0;

        for inst_lock in instances.values() {
            let inst = inst_lock.read().await;
            let (v, s) = self.get_memory_for_instance(&inst).await;
            vram += v;
            sysmem += s;
        }
        (vram, sysmem)
    }

    /// Get memory estimate for an instance. Prefers live sample, falls back to learned values.
    async fn get_memory_for_instance(&self, inst: &Instance) -> (u64, u64) {
        // Try live sample first if process is running
        if let Some(pid) = inst.get_pid() {
            let (live_vram, live_sysmem) = self.memory_sampler.sample(pid);
            if live_vram > 0 || live_sysmem > 0 {
                return (live_vram, live_sysmem);
            }
        }

        // Fall back to learned values
        if let Some((vram, sysmem)) = self.metrics.get_learned_memory(&inst.args_hash).await {
            return (vram, sysmem);
        }

        // Unknown - return 0 (should rarely happen after cold-start blocking)
        (0, 0)
    }

    /// Get memory estimate for spawning a new instance with given args_hash.
    async fn get_memory_estimate(&self, args_hash: &str) -> (u64, u64) {
        if let Some((vram, sysmem)) = self.metrics.get_learned_memory(args_hash).await {
            (vram, sysmem)
        } else {
            // Unknown args - return 0, will learn after spawn
            (0, 0)
        }
    }

    /// Get memory estimate for a model:profile key (for routing decisions).
    /// Returns (vram_mb, sysmem_mb) - (0, 0) if unknown.
    async fn get_memory_estimate_for_key(&self, key: &str) -> (u64, u64) {
        if let Some(args_hash) = self.get_args_hash_for_key(key).await {
            self.get_memory_estimate(&args_hash).await
        } else {
            (0, 0)
        }
    }

    // Helper to update peak memory for instances of a model (based on learned/live values)
    async fn update_peak_memory(&self, model_name: &str, profile_id: &str) {
        let instances = self.instances.read().await;
        let display_name = format!("{}:{}", model_name, profile_id);

        for inst_lock in instances.values() {
            let inst = inst_lock.read().await;
            if inst.model_name == model_name && inst.profile_id == profile_id {
                let (v, s) = self.get_memory_for_instance(&inst).await;
                if v > 0 || s > 0 {
                    let hash_metrics = self.metrics.get_hash_metrics(&inst.args_hash).await;
                    hash_metrics.observe_memory(v, s);
                    hash_metrics.add_display_name(&display_name);
                }
            }
        }
    }

    /// Stop an instance and perform cleanup: log termination, stop process, update metrics.
    /// reason: "idle", "error", "manual", or "capacity"
    async fn stop_and_cleanup_instance(&self, inst: &Instance, reason: &str) {
        let (vram, sysmem) = self.get_memory_for_instance(inst).await;

        match reason {
            "error" => {
                warn!(
                    event = "instance_terminate",
                    reason = "error",
                    instance_id = %inst.id,
                    model = %inst.model_name,
                    profile = %inst.profile_id,
                    freed_vram_mb = vram,
                    freed_sysmem_mb = sysmem,
                    "Removing crashed instance"
                );
            }
            "capacity" => {
                info!(
                    event = "instance_terminate",
                    reason = "capacity",
                    instance_id = %inst.id,
                    model = %inst.model_name,
                    profile = %inst.profile_id,
                    freed_vram_mb = vram,
                    freed_sysmem_mb = sysmem,
                    "Evicting instance to free capacity"
                );
            }
            _ => {
                info!(
                    event = "instance_terminate",
                    reason = reason,
                    instance_id = %inst.id,
                    model = %inst.model_name,
                    profile = %inst.profile_id,
                    freed_vram_mb = vram,
                    freed_sysmem_mb = sysmem,
                    "Stopping instance"
                );
            }
        }

        if let Err(e) = inst.stop().await {
            error!("Failed to stop instance {}: {}", inst.id, e);
        }

        self.update_peak_memory(&inst.model_name, &inst.profile_id)
            .await;

        // Trigger instant gossip so peers learn about freed capacity immediately
        self.gossip_trigger.notify_one();
    }

    // Helper to identify victims for eviction. Needs to be called under lock if we want to be sure.
    // But since we want to release lock to stop instances, we return victims.
    // This method does NOT remove them from the map or stop them.
    async fn pick_victims(
        &self,
        instances_map: &HashMap<String, Arc<RwLock<Instance>>>,
        required_vram: u64,
        required_sysmem: u64,
    ) -> Result<Vec<String>, NodeError> {
        let mut curr_vram = 0;
        let mut curr_sysmem = 0;

        struct Candidate {
            id: String,
            last_activity: std::time::Instant,
            vram: u64,
            sysmem: u64,
        }

        let mut candidates = Vec::new();

        for (id, inst_lock) in instances_map.iter() {
            let inst = inst_lock.read().await;
            let (inst_vram, inst_sysmem) = self.get_memory_for_instance(&inst).await;

            curr_vram += inst_vram;
            curr_sysmem += inst_sysmem;

            // Can only evict idle instances
            if inst.in_flight_requests == 0 {
                candidates.push(Candidate {
                    id: id.clone(),
                    last_activity: inst.last_activity,
                    vram: inst_vram,
                    sysmem: inst_sysmem,
                });
            }
        }

        let max_vram = self.config.max_vram_mb;
        let max_sysmem = self.config.max_sysmem_mb;

        if curr_vram + required_vram <= max_vram && curr_sysmem + required_sysmem <= max_sysmem {
            return Ok(Vec::new());
        }

        candidates.sort_by_key(|c| c.last_activity);

        let idle_count = candidates.len();
        let mut victims = Vec::new();

        for candidate in candidates {
            curr_vram -= candidate.vram;
            curr_sysmem -= candidate.sysmem;
            victims.push(candidate.id);

            if curr_vram + required_vram <= max_vram && curr_sysmem + required_sysmem <= max_sysmem
            {
                return Ok(victims);
            }
        }

        // Log detailed info about why resources are insufficient
        let busy_count = instances_map.len() - idle_count;
        warn!(
            event = "insufficient_resources",
            curr_vram_mb = curr_vram,
            curr_sysmem_mb = curr_sysmem,
            required_vram_mb = required_vram,
            required_sysmem_mb = required_sysmem,
            max_vram_mb = max_vram,
            max_sysmem_mb = max_sysmem,
            total_instances = instances_map.len(),
            busy_instances = busy_count,
            idle_candidates = idle_count,
            "Cannot free enough resources for new instance"
        );
        Err(NodeError::InsufficientResources)
    }

    /// Evict all idle instances (cold-start OOM stage 1).
    /// Returns the count of evicted instances.
    async fn evict_all_idle_instances(&self) -> usize {
        // First pass: identify idle instances under read lock
        let mut to_remove = Vec::new();
        {
            let instances = self.instances.read().await;
            for (id, inst_lock) in instances.iter() {
                let inst = inst_lock.read().await;
                if inst.in_flight_requests == 0
                    && !inst.draining.load(std::sync::atomic::Ordering::Relaxed)
                {
                    to_remove.push((id.clone(), inst.port));
                }
            }
        }

        if to_remove.is_empty() {
            return 0;
        }

        info!(
            event = "cold_oom_evict_stage1",
            count = to_remove.len(),
            "Evicting all idle instances for cold-start OOM recovery"
        );

        // Second pass: remove under write lock, re-verify idleness to close TOCTOU gap
        let removed: Vec<(u16, Arc<RwLock<Instance>>)>;
        {
            let mut instances = self.instances.write().await;
            let mut verified_removals = Vec::new();
            for (id, port) in to_remove {
                if let Some(inst_lock) = instances.get(&id) {
                    let inst = inst_lock.read().await;
                    // Re-verify: instance may have become active between read and write lock
                    if inst.in_flight_requests == 0 {
                        drop(inst);
                        if let Some(removed_inst) = instances.remove(&id) {
                            verified_removals.push((port, removed_inst));
                        }
                    }
                }
            }
            removed = verified_removals;
        }
        // Write lock released here

        let evicted_count = removed.len();

        // Now do cleanup without holding any instance map lock
        for (port, inst_lock) in &removed {
            self.release_port(*port).await;
            let inst = inst_lock.read().await;
            self.stop_and_cleanup_instance(&inst, "cold_oom_stage1")
                .await;
        }

        self.notify_all_queues().await;
        evicted_count
    }

    /// Gracefully evict all instances (cold-start OOM stage 2).
    /// Marks all instances as draining and waits for active requests to complete.
    async fn evict_all_instances_gracefully(&self) {
        // First, mark all instances as draining to prevent new requests
        let mut instances_to_drain: Vec<(String, u16)> = Vec::new();
        {
            let instances = self.instances.read().await;
            for (id, inst_lock) in instances.iter() {
                let inst = inst_lock.read().await;
                inst.draining
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                instances_to_drain.push((id.clone(), inst.port));
            }
        }

        if instances_to_drain.is_empty() {
            return;
        }

        info!(
            event = "cold_oom_evict_stage2_start",
            count = instances_to_drain.len(),
            "Draining all instances for cold-start OOM recovery (waiting for active requests)"
        );

        // Wait for all instances to become idle with timeout to prevent indefinite blocking
        // (e.g., slowloris-style attacks or hung clients)
        // Use shutdown_grace_period_seconds as the timeout - if OOM recovery takes longer,
        // force eviction to prevent complete service unavailability
        let drain_timeout = Duration::from_secs(self.config.shutdown_grace_period_seconds);
        let drain_start = std::time::Instant::now();
        let mut last_log = drain_start;
        loop {
            // Check timeout first
            if drain_start.elapsed() >= drain_timeout {
                warn!(
                    event = "cold_oom_evict_stage2_timeout",
                    elapsed_secs = drain_start.elapsed().as_secs(),
                    timeout_secs = self.config.shutdown_grace_period_seconds,
                    "Drain timeout exceeded during OOM recovery, forcing eviction"
                );
                break;
            }

            let mut all_idle = true;
            let mut total_in_flight: usize = 0;
            {
                let instances = self.instances.read().await;
                for (id, _) in &instances_to_drain {
                    if let Some(inst_lock) = instances.get(id) {
                        let inst = inst_lock.read().await;
                        if inst.in_flight_requests > 0 {
                            all_idle = false;
                            total_in_flight += inst.in_flight_requests;
                        }
                    }
                }
            }

            if all_idle {
                break;
            }

            // Log progress every 10 seconds
            let now = std::time::Instant::now();
            if now.duration_since(last_log) >= Duration::from_secs(10) {
                warn!(
                    event = "cold_oom_evict_stage2_waiting",
                    elapsed_secs = drain_start.elapsed().as_secs(),
                    timeout_secs = self.config.shutdown_grace_period_seconds,
                    in_flight_requests = total_in_flight,
                    instances_to_drain = instances_to_drain.len(),
                    "Still waiting for in-flight requests to complete during OOM recovery"
                );
                last_log = now;
            }

            // Wait a bit before checking again
            tokio::time::sleep(Duration::from_millis(100)).await;
        }

        info!(
            event = "cold_oom_evict_stage2_drain_complete",
            "All instances drained, proceeding with eviction"
        );

        // Now evict all instances - collect under lock, cleanup after releasing
        let removed: Vec<(u16, Arc<RwLock<Instance>>)>;
        {
            let mut instances = self.instances.write().await;
            removed = instances_to_drain
                .iter()
                .filter_map(|(id, port)| instances.remove(id).map(|inst_lock| (*port, inst_lock)))
                .collect();
        }
        // Write lock released

        // Cleanup without holding the lock
        for (port, inst_lock) in &removed {
            self.release_port(*port).await;
            let inst = inst_lock.read().await;
            self.stop_and_cleanup_instance(&inst, "cold_oom_stage2")
                .await;
        }

        self.notify_all_queues().await;
    }

    /// Mark all running instances as draining after a binary swap.
    ///
    /// Draining instances will not receive new requests (checked in instance selection).
    /// The background eviction loop (`check_idle_instances`) will terminate them once their
    /// in-flight request count drops to 0. New requests will spawn fresh instances using
    /// the new binary.
    pub async fn drain_instances_for_binary_update(&self) {
        let instances = self.instances.read().await;
        let mut drained_count = 0usize;
        for (id, inst_lock) in instances.iter() {
            let inst = inst_lock.read().await;
            // Only drain instances that aren't already draining
            if !inst.draining.load(std::sync::atomic::Ordering::Relaxed) {
                inst.draining
                    .store(true, std::sync::atomic::Ordering::Relaxed);
                drained_count += 1;
                info!(
                    event = "instance_drain_binary_update",
                    instance_id = %id,
                    model = %inst.model_name,
                    profile = %inst.profile_id,
                    in_flight = inst.in_flight_requests,
                    "Marked instance as draining after binary swap"
                );
            }
        }
        drop(instances);

        if drained_count > 0 {
            info!(
                event = "binary_update_drain_started",
                count = drained_count,
                "Marked all running instances as draining for binary update"
            );
            // Notify capacity change so waiting requests can spawn new instances
            // with the updated binary
            self.capacity_notify.notify_waiters();
            self.notify_all_queues().await;
        }
    }

    pub async fn get_instance_for_model(
        &self,
        model_name: &str,
        profile: &Profile,
        reserve_slot: bool,
    ) -> Result<Arc<RwLock<Instance>>, NodeError> {
        if !reserve_slot {
            return self
                .try_get_or_spawn(model_name, profile, false, "prewarm", false)
                .await;
        }

        let loop_start = std::time::Instant::now();
        let queue_timeout = self.queue_timeout_duration(profile); // None = infinite wait

        // Track cold-start OOM retry stage:
        // 0 = no failure yet
        // 1 = first failure, will evict idle instances
        // 2 = second failure (after evicting idle), will evict all gracefully
        let mut cold_retry_stage = 0u8;

        // Track consecutive spawn failures for exponential backoff.
        // Prevents tight crash loops when the binary is fundamentally broken
        // (e.g., missing shared libraries, bad binary path).
        let mut consecutive_spawn_failures = 0u32;

        // Token for this request - None = fresh request, Some = notified waiter with priority
        let mut my_token: Option<u64> = None;
        // Track when we first started waiting in queue (for metrics)
        let mut queue_start: Option<std::time::Instant> = None;

        loop {
            // After repeated spawn failures, give up on local spawning and return
            // an error so the router can try forwarding to a peer node.
            // 3 attempts covers cold-start recovery (evict idle, evict all) plus
            // one more try — if it still fails, the binary is likely broken.
            const MAX_LOCAL_SPAWN_FAILURES: u32 = 3;
            if consecutive_spawn_failures >= MAX_LOCAL_SPAWN_FAILURES {
                if let Some(token) = my_token {
                    self.remove_pending_token(model_name, &profile.id, token)
                        .await;
                }
                warn!(
                    event = "spawn_failures_exhausted",
                    model = %model_name,
                    profile = %profile.id,
                    failures = consecutive_spawn_failures,
                    "Giving up on local spawning after repeated failures"
                );
                return Err(NodeError::SpawnFailuresExhausted);
            }
            // If node is draining, stop waiting and let the request fail fast
            if self.draining.load(Ordering::Relaxed) {
                if let Some(token) = my_token {
                    self.remove_pending_token(model_name, &profile.id, token)
                        .await;
                }
                return Err(NodeError::Other(anyhow::anyhow!("Node is shutting down")));
            }
            // Check timeout only if configured (None = infinite wait)
            if let Some(timeout) = queue_timeout {
                if loop_start.elapsed() > timeout {
                    // Clean up pending token if we have one
                    if let Some(token) = my_token {
                        self.remove_pending_token(model_name, &profile.id, token)
                            .await;
                    }
                    return Err(NodeError::QueueTimeout);
                }
            }

            // FAIRNESS CHECK: Fresh requests must yield to queued/pending waiters
            // Notified waiters (with token) skip this check to claim their priority slot
            if my_token.is_none() && self.has_pending_waiters(model_name, &profile.id).await {
                // There are waiters ahead of us - join the queue instead of trying to grab slot
                let key = format!("{}:{}", model_name, profile.id);
                let (tx, rx) = oneshot::channel();

                {
                    // LOCK_ORDER: queues (2)
                    let mut queues = self.queues.write().await;

                    // Check global queue limit first (if configured)
                    if self.config.max_total_queue_entries > 0 {
                        let total: usize = queues.values().map(|q| q.len()).sum();
                        if total >= self.config.max_total_queue_entries {
                            info!(event = "queue_drop", reason = "global_limit", model = %model_name, total = total);
                            return Err(NodeError::QueueFull);
                        }
                    }

                    let queue = queues.entry(key.clone()).or_insert_with(VecDeque::new);
                    let max_queue = profile.effective_max_queue_size(&self.config.model_defaults);
                    // 0 = unlimited queue size
                    if max_queue > 0 && queue.len() >= max_queue {
                        info!(event = "queue_drop", reason = "full", model = %model_name);
                        return Err(NodeError::QueueFull);
                    }
                    queue.push_back(tx);
                    // Start queue wait timer on first enqueue
                    if queue_start.is_none() {
                        queue_start = Some(std::time::Instant::now());
                    }
                    // Warn about large queues (potential memory pressure)
                    let queue_len = queue.len();
                    if queue_len > 100 && queue_len % 100 == 0 {
                        warn!(event = "queue_large", model = %model_name, queue_length = queue_len,
                            "Queue growing large - consider increasing capacity");
                    }
                    info!(event = "queue_enqueue", model = %model_name, queue_length = queue_len, reason = "fairness");
                }

                // Wait for queue notification with optional timeout
                let wait_result = match queue_timeout {
                    Some(timeout) => {
                        let elapsed = loop_start.elapsed();
                        if elapsed >= timeout {
                            info!(event = "queue_drop", reason = "timeout", model = %model_name);
                            return Err(NodeError::QueueTimeout);
                        }
                        let remaining = timeout - elapsed;
                        match tokio::time::timeout(remaining, rx).await {
                            Ok(result) => result,
                            Err(_) => {
                                info!(event = "queue_drop", reason = "timeout", model = %model_name);
                                return Err(NodeError::QueueTimeout);
                            }
                        }
                    }
                    None => rx.await, // Infinite wait
                };

                match wait_result {
                    Ok(token) => {
                        my_token = Some(token);
                        continue; // Now retry with priority token
                    }
                    Err(_) => return Err(NodeError::QueueError),
                }
            }

            match self
                .try_get_or_spawn(model_name, profile, true, "demand", my_token.is_some())
                .await
            {
                Ok(inst) => {
                    // Successfully acquired slot - clean up pending token and record metrics
                    if let Some(token) = my_token.take() {
                        self.remove_pending_token(model_name, &profile.id, token)
                            .await;
                    }
                    // Record queue wait time if we waited
                    if let Some(start) = queue_start {
                        self.metrics
                            .observe_queue_wait(start.elapsed().as_millis() as u64);
                    }

                    let (ready_signal, is_cold_start) = {
                        let r = inst.read().await;
                        let s = r.status.lock().clone();
                        if s == InstanceStatus::Ready {
                            return Ok(inst.clone());
                        }
                        if s == InstanceStatus::Failed {
                            // Instance failed immediately - check for cold-start OOM recovery
                            let is_cold = r.is_cold_start;
                            drop(r);
                            let mut w = inst.write().await;
                            if w.in_flight_requests > 0 {
                                w.in_flight_requests -= 1;
                            }
                            drop(w);

                            consecutive_spawn_failures += 1;

                            if is_cold {
                                cold_retry_stage += 1;
                                if cold_retry_stage == 1 {
                                    // Stage 1: evict all idle instances
                                    let evicted = self.evict_all_idle_instances().await;
                                    if evicted > 0 {
                                        info!(
                                            event = "cold_oom_retry",
                                            stage = 1,
                                            model = %model_name,
                                            evicted = evicted,
                                            "Retrying after evicting idle instances"
                                        );
                                        continue;
                                    }
                                    // No idle instances - escalate to stage 2
                                    cold_retry_stage = 2;
                                }

                                if cold_retry_stage == 2 {
                                    // Stage 2: gracefully evict all (wait for active)
                                    info!(
                                        event = "cold_oom_retry",
                                        stage = 2,
                                        model = %model_name,
                                        "Escalating to graceful eviction of all instances"
                                    );
                                    self.evict_all_instances_gracefully().await;
                                    continue;
                                }
                            }

                            // Stage 3: Wait indefinitely for capacity (never reject)
                            // This can happen if eviction freed resources but another request claimed them,
                            // or if the model truly requires more resources than the node has (config error).
                            warn!(
                                event = "waiting_for_resources",
                                model = %model_name,
                                profile = %profile.id,
                                "All retry stages exhausted, waiting for cluster capacity"
                            );
                            self.capacity_notify.notified().await;
                            continue; // Retry the loop
                        }
                        (r.ready_signal.clone(), r.is_cold_start)
                    };

                    // Wait for ready signal
                    ready_signal.notified().await;

                    // Check status again
                    let r = inst.read().await;
                    let s = r.status.lock().clone();
                    if s == InstanceStatus::Ready {
                        return Ok(inst.clone());
                    } else {
                        // Instance failed after startup - check for cold-start OOM recovery
                        let is_cold = is_cold_start;
                        drop(r);
                        let mut w = inst.write().await;
                        if w.in_flight_requests > 0 {
                            w.in_flight_requests -= 1;
                        }
                        drop(w);

                        consecutive_spawn_failures += 1;

                        if is_cold {
                            cold_retry_stage += 1;
                            if cold_retry_stage == 1 {
                                // Stage 1: evict all idle instances
                                let evicted = self.evict_all_idle_instances().await;
                                if evicted > 0 {
                                    info!(
                                        event = "cold_oom_retry",
                                        stage = 1,
                                        model = %model_name,
                                        evicted = evicted,
                                        "Retrying after evicting idle instances"
                                    );
                                    continue;
                                }
                                // No idle instances - escalate to stage 2
                                cold_retry_stage = 2;
                            }

                            if cold_retry_stage == 2 {
                                // Stage 2: gracefully evict all (wait for active)
                                info!(
                                    event = "cold_oom_retry",
                                    stage = 2,
                                    model = %model_name,
                                    "Escalating to graceful eviction of all instances"
                                );
                                self.evict_all_instances_gracefully().await;
                                continue;
                            }
                        }

                        // Stage 3: Wait indefinitely for capacity (never reject)
                        warn!(
                            event = "waiting_for_resources",
                            model = %model_name,
                            profile = %profile.id,
                            "All retry stages exhausted after startup failure, waiting for cluster capacity"
                        );
                        self.capacity_notify.notified().await;
                        continue; // Retry the loop
                    }
                }
                Err(e) => {
                    // Determine the reason string for logging
                    let spawn_error_reason = match &e {
                        NodeError::MaxInstancesProfile => "max_instances_profile",
                        NodeError::MaxInstancesNode => "max_instances_node",
                        NodeError::QueueFull => "queue_full",
                        NodeError::InsufficientResources => "insufficient_resources",
                        NodeError::YieldToWaiters => "yield_to_waiters",
                        _ => "other",
                    };

                    if matches!(
                        e,
                        NodeError::MaxInstancesProfile
                            | NodeError::MaxInstancesNode
                            | NodeError::QueueFull
                            | NodeError::InsufficientResources
                            | NodeError::YieldToWaiters
                    ) {
                        info!(
                            event = "spawn_blocked",
                            model = %model_name,
                            profile = %profile.id,
                            reason = spawn_error_reason,
                            "Instance spawn blocked, queueing request"
                        );

                        // If we had a token but failed to acquire, clear it before re-queueing
                        if let Some(token) = my_token.take() {
                            self.remove_pending_token(model_name, &profile.id, token)
                                .await;
                        }

                        let key = format!("{}:{}", model_name, profile.id);
                        let (tx, rx) = oneshot::channel();

                        {
                            // LOCK_ORDER: queues (2)
                            let mut queues = self.queues.write().await;

                            // Check global queue limit first (if configured)
                            if self.config.max_total_queue_entries > 0 {
                                let total: usize = queues.values().map(|q| q.len()).sum();
                                if total >= self.config.max_total_queue_entries {
                                    info!(event = "queue_drop", reason = "global_limit", model = %model_name, total = total);
                                    return Err(NodeError::QueueFull);
                                }
                            }

                            let queue = queues.entry(key.clone()).or_insert_with(VecDeque::new);
                            let max_queue = profile.effective_max_queue_size(&self.config.model_defaults);
                            // 0 = unlimited queue size
                            if max_queue > 0 && queue.len() >= max_queue {
                                info!(event = "queue_drop", reason = "full", model = %model_name);
                                return Err(NodeError::QueueFull);
                            }
                            queue.push_back(tx);
                            // Start queue wait timer on first enqueue
                            if queue_start.is_none() {
                                queue_start = Some(std::time::Instant::now());
                            }
                            // Warn about large queues (potential memory pressure)
                            let queue_len = queue.len();
                            if queue_len > 100 && queue_len % 100 == 0 {
                                warn!(event = "queue_large", model = %model_name, queue_length = queue_len,
                                    "Queue growing large - consider increasing capacity");
                            }
                            info!(event = "queue_enqueue", model = %model_name, queue_length = queue_len, spawn_error = spawn_error_reason);
                        }

                        // Track that this model needs eviction to spawn (outside queues lock)
                        if matches!(e, NodeError::InsufficientResources | NodeError::MaxInstancesNode) {
                            self.needs_eviction.write().await.insert(key.clone());
                        }

                        // Wait for queue notification with optional timeout
                        let wait_result = match queue_timeout {
                            Some(timeout) => {
                                let elapsed = loop_start.elapsed();
                                if elapsed >= timeout {
                                    info!(event = "queue_drop", reason = "timeout", model = %model_name);
                                    return Err(NodeError::QueueTimeout);
                                }
                                let remaining = timeout - elapsed;
                                match tokio::time::timeout(remaining, rx).await {
                                    Ok(result) => result,
                                    Err(_) => {
                                        info!(event = "queue_drop", reason = "timeout", model = %model_name);
                                        return Err(NodeError::QueueTimeout);
                                    }
                                }
                            }
                            None => rx.await, // Infinite wait
                        };

                        match wait_result {
                            Ok(token) => {
                                my_token = Some(token);
                                continue;
                            }
                            Err(_) => return Err(NodeError::QueueError),
                        }
                    } else {
                        // Clean up token for non-retriable errors
                        if let Some(token) = my_token {
                            self.remove_pending_token(model_name, &profile.id, token)
                                .await;
                        }
                        return Err(e);
                    }
                }
            }
        }
    }

    /// Get queue timeout duration. Returns None for infinite wait.
    fn queue_timeout_duration(&self, profile: &Profile) -> Option<Duration> {
        effective_queue_timeout(profile, &self.config.model_defaults)
    }

    /// Compute args_hash for a "model:profile" or "model" key.
    /// Returns None if model/profile not found in cookbook.
    pub async fn get_args_hash_for_key(&self, key: &str) -> Option<String> {
        get_args_hash_for_key(&self.cookbook, key).await
    }

    async fn try_get_or_spawn(
        &self,
        model_name: &str,
        profile: &Profile,
        reserve_slot: bool,
        reason: &str,
        has_priority_token: bool,
    ) -> Result<Arc<RwLock<Instance>>, NodeError> {
        // Loop for lock-free eviction retry logic
        loop {
            // 1. Acquire Write Lock
            let mut instances_map = self.instances.write().await;

            // FAIRNESS: Re-check pending waiters under lock to close TOCTOU gap.
            // Fresh requests (no priority token) must yield if notified waiters exist.
            if reserve_slot && !has_priority_token && self.has_pending_waiters(model_name, &profile.id).await {
                return Err(NodeError::YieldToWaiters);
            }

            // 2. Prune crashed instances before making scheduling decisions.
            let mut crashed_instances = Vec::new();
            for (id, inst_lock) in instances_map.iter() {
                let inst = inst_lock.read().await;
                if !inst.is_alive() {
                    warn!(
                        instance_id = %inst.id,
                        model = %inst.model_name,
                        profile = %inst.profile_id,
                        "Detected crashed instance; reclaiming resources"
                    );
                    crashed_instances.push((
                        id.clone(),
                        inst.model_name.clone(),
                        inst.profile_id.clone(),
                        inst.port,
                    ));
                }
            }

            if !crashed_instances.is_empty() {
                // Remove instances and collect ports for release
                let ports_to_release: Vec<_> = crashed_instances
                    .iter()
                    .map(|(_, _, _, port)| *port)
                    .collect();
                for (id, _, _, _) in &crashed_instances {
                    instances_map.remove(id);
                }

                // Release ports BEFORE dropping instances lock to prevent race
                for port in &ports_to_release {
                    self.release_port(*port).await;
                }
                drop(instances_map);

                for (_, m, p, _) in crashed_instances {
                    self.update_peak_memory(&m, &p).await;
                }
                self.notify_all_queues().await;

                continue;
            }

            // 2. Check for existing available instance (pick fewest in-flight)
            // Skip instances marked as draining (e.g., after a binary swap) — they should
            // only finish their in-flight requests and then be terminated.
            let max_concurrent = self.config.model_defaults.max_concurrent_requests_per_instance;
            let mut best_instance: Option<Arc<tokio::sync::RwLock<Instance>>> = None;
            let mut best_in_flight = usize::MAX;
            for inst_lock in instances_map.values() {
                if reserve_slot {
                    let inst = inst_lock.read().await;
                    if inst.model_name == model_name && inst.profile_id == profile.id {
                        if !inst.is_alive() {
                            continue;
                        }
                        if inst.draining.load(std::sync::atomic::Ordering::Relaxed) {
                            continue;
                        }
                        let has_capacity = max_concurrent == 0
                            || inst.in_flight_requests < max_concurrent;
                        if has_capacity && inst.in_flight_requests < best_in_flight {
                            best_in_flight = inst.in_flight_requests;
                            best_instance = Some(inst_lock.clone());
                        }
                    }
                } else {
                    let inst = inst_lock.read().await;
                    if inst.model_name == model_name
                        && inst.profile_id == profile.id
                        && !inst.draining.load(std::sync::atomic::Ordering::Relaxed)
                    {
                        return Ok(inst_lock.clone());
                    }
                }
            }
            if let Some(inst_lock) = best_instance {
                let mut inst = inst_lock.write().await;
                // Re-check capacity and draining after upgrading to write lock
                let still_has_capacity = max_concurrent == 0
                    || inst.in_flight_requests < max_concurrent;
                let is_draining = inst.draining.load(std::sync::atomic::Ordering::Relaxed);
                if still_has_capacity && inst.is_alive() && !is_draining {
                    inst.in_flight_requests += 1;
                    inst.last_activity = std::time::Instant::now();
                    return Ok(inst_lock.clone());
                }
                // Lost the race or instance started draining — fall through to spawn/queue
            }

            // 3. Check profile max instances
            let mut profile_count = 0;
            for inst_lock in instances_map.values() {
                let inst = inst_lock.read().await;
                if inst.model_name == model_name && inst.profile_id == profile.id {
                    profile_count += 1;
                }
            }
            let max_instances = profile.effective_max_instances(&self.config.model_defaults);
            if profile_count >= max_instances {
                return Err(NodeError::MaxInstancesProfile);
            }

            // 4. Build args early to compute args_hash for memory estimation
            // This is needed before pick_victims to know required memory
            let (pre_args, _model_arg_present, _hf_repo_arg_present) = build_pre_args(profile);
            let args_hash = compute_args_hash(&pre_args);
            let (required_vram, required_sysmem) = self.get_memory_estimate(&args_hash).await;

            // 4. Check Memory, Node Guardrails & Identify Victims
            let mut victims = self
                .pick_victims(&instances_map, required_vram, required_sysmem)
                .await?;

            let current_instances = instances_map.len();
            let max_node_instances = self.config.max_instances_per_node;

            if current_instances + 1 > max_node_instances {
                let mut additional_needed = current_instances + 1 - max_node_instances;
                if additional_needed > victims.len() {
                    additional_needed -= victims.len();
                } else {
                    additional_needed = 0;
                }

                if additional_needed > 0 {
                    let mut idle_candidates = Vec::new();
                    for (id, inst_lock) in instances_map.iter() {
                        if victims.iter().any(|existing| existing == id) {
                            continue;
                        }
                        let inst = inst_lock.read().await;
                        if inst.in_flight_requests == 0 {
                            idle_candidates.push((id.clone(), inst.last_activity));
                        }
                    }

                    idle_candidates.sort_by_key(|(_, last_activity)| *last_activity);

                    for (id, _) in idle_candidates {
                        victims.push(id.clone());
                        additional_needed = additional_needed.saturating_sub(1);
                        if additional_needed == 0 {
                            break;
                        }
                    }
                }

                if additional_needed > 0 {
                    warn!(
                        "Max instances per node ({}) reached; cannot spawn additional instances",
                        max_node_instances
                    );
                    return Err(NodeError::MaxInstancesNode);
                }
            }

            if !victims.is_empty() {
                // Remove victims from the map before we drop the global lock.
                // Re-verify each victim is still idle to prevent race condition where
                // a request was assigned between pick_victims() and now.
                let mut removed_instances = Vec::new();
                for id in victims {
                    // Re-verify victim is still idle before removal
                    if let Some(inst_lock) = instances_map.get(&id) {
                        let inst = inst_lock.read().await;
                        if inst.in_flight_requests > 0 {
                            // Victim became active between pick_victims and now, skip it
                            info!(
                                instance_id = %id,
                                in_flight = %inst.in_flight_requests,
                                "Victim became active during eviction, skipping"
                            );
                            continue;
                        }
                        drop(inst);
                    }
                    // Now safe to remove
                    if let Some(inst_lock) = instances_map.remove(&id) {
                        removed_instances.push(inst_lock);
                    }
                }

                // Collect ports and release them BEFORE dropping instances lock
                let mut ports_to_release: Vec<u16> = Vec::new();
                for inst_lock in &removed_instances {
                    let inst = inst_lock.read().await;
                    ports_to_release.push(inst.port);
                }
                for port in &ports_to_release {
                    self.release_port(*port).await;
                }
                drop(instances_map);

                // Stop instances (they're already removed from map)
                // Note: We do NOT call notify_queue here. The current request triggered
                // the eviction and will continue looping to claim the freed capacity.
                // Notifying here would wake waiters for the EVICTED instance's profile,
                // causing a race where they spawn the wrong profile and get immediately
                // evicted again (profile thrashing bug).
                for inst_lock in removed_instances {
                    let inst = inst_lock.read().await;
                    self.stop_and_cleanup_instance(&inst, "capacity").await;
                }

                // Restart loop to grab the space we just freed
                continue;
            }

            // 5. Spawn Preparation
            // Parse --host and --port overrides from profile args
            let mut host = "127.0.0.1".to_string();
            let mut host_overridden = false;
            let mut port_override: Option<u16> = None;

            let mut iter = profile.llama_server_args.iter();
            while let Some(arg) = iter.next() {
                if arg == "--host" {
                    if let Some(h) = iter.next() {
                        host = h.clone();
                        host_overridden = true;
                    } else {
                        return Err(NodeError::MissingHostArg(profile.id.clone()));
                    }
                } else if let Some(val) = arg.strip_prefix("--host=") {
                    host = val.to_string();
                    host_overridden = true;
                }

                if arg == "--port" {
                    if let Some(p) = iter.next() {
                        let parsed = p.parse::<u16>().map_err(|_| {
                            NodeError::InvalidPortArg(p.clone(), profile.id.clone())
                        })?;
                        port_override = Some(parsed);
                    } else {
                        return Err(NodeError::MissingPortArg(profile.id.clone()));
                    }
                } else if let Some(val) = arg.strip_prefix("--port=") {
                    let parsed = val.parse::<u16>().map_err(|_| {
                        NodeError::InvalidPortArg(val.to_string(), profile.id.clone())
                    })?;
                    port_override = Some(parsed);
                }
            }

            // Release outer write lock before entering retry loop (will re-acquire as needed)
            drop(instances_map);

            // Retry loop for port allocation and spawn (handles TOCTOU race on ports)
            const MAX_SPAWN_RETRIES: usize = 3;
            let mut spawn_attempt = 0;
            let (instance_id, port, inst_arc, is_cold_start, final_args) = loop {
                spawn_attempt += 1;

                let instance_id = Ulid::new().to_string();
                let port_overridden = port_override.is_some();
                let port = match port_override {
                    Some(p) => {
                        self.reserve_specific_port(p).await?;
                        p
                    }
                    None => match self.get_available_port().await {
                        Ok(p) => p,
                        Err(e) => {
                            error!("Failed to get available port: {}", e);
                            return Err(e);
                        }
                    },
                };

                // Build final args: pre_args + host/port
                let mut args = pre_args.clone();
                if !host_overridden {
                    args.push("--host".to_string());
                    args.push(host.clone());
                }
                if !port_overridden {
                    args.push("--port".to_string());
                    args.push(port.to_string());
                }

                // Check if this is a cold start (no learned memory for this args_hash)
                let is_cold_start = !self.metrics.has_memory_data(&args_hash).await;

                let new_instance = Instance::new(
                    instance_id.clone(),
                    model_name.to_string(),
                    profile.id.clone(),
                    host.clone(),
                    port,
                    args_hash.clone(),
                    is_cold_start,
                );
                let inst_arc = Arc::new(RwLock::new(new_instance));

                // Try to spawn BEFORE inserting into map to avoid race condition
                // where is_alive() returns false because child is None
                let spawn_result = {
                    let inst = inst_arc.read().await;
                    inst.spawn(&self.config.llama_cpp.binary_path, &args)
                };

                match spawn_result {
                    Ok(()) => {
                        // Spawn succeeded - NOW insert into map and update peak memory
                        let mut instances_map = self.instances.write().await;

                        // Re-check capacity constraints to close race window between
                        // dropping lock (line 1368) and re-acquiring here. Another spawn
                        // could have completed in that window.
                        let current_count = instances_map.len();
                        let max_node = self.config.max_instances_per_node;
                        if current_count >= max_node {
                            // Race detected: another spawn filled capacity while we were spawning
                            drop(instances_map);
                            self.release_port(port).await;
                            // Kill the process we just spawned
                            let inst = inst_arc.read().await;
                            let _ = inst.stop().await;
                            warn!(
                                event = "spawn_race_detected",
                                model = %model_name,
                                profile = %profile.id,
                                current = current_count,
                                max = max_node,
                                "Killed just-spawned instance due to capacity race"
                            );
                            return Err(NodeError::MaxInstancesNode);
                        }

                        instances_map.insert(instance_id.clone(), inst_arc.clone());
                        drop(instances_map);
                        self.update_peak_memory(model_name, &profile.id).await;
                        // Success - break out of retry loop with args
                        break (instance_id, port, inst_arc, is_cold_start, args);
                    }
                    Err(e) => {
                        error!(
                            event = "instance_spawn_failed",
                            attempt = spawn_attempt,
                            max_attempts = MAX_SPAWN_RETRIES,
                            port = port,
                            error = %e,
                            "Failed to start new instance"
                        );

                        // Cleanup the failed attempt - just release port (instance was never in map)
                        self.release_port(port).await;

                        // If we've exhausted retries or have a port override (can't try different port), fail
                        if spawn_attempt >= MAX_SPAWN_RETRIES || port_override.is_some() {
                            return Err(NodeError::Other(e));
                        }

                        // Otherwise retry with a new port
                        warn!(
                            event = "instance_spawn_retry",
                            attempt = spawn_attempt + 1,
                            "Retrying instance spawn with new port"
                        );
                    }
                }
            };
            // Suppress unused warning - port is logged in events
            let _ = port;

            let inst_clone = inst_arc.clone();
            let startup_timeout = profile.effective_startup_timeout_seconds();
            let eviction_tenure_secs = profile.effective_min_eviction_tenure_secs(&self.config.model_defaults);
            let api_key = final_args
                .iter()
                .position(|arg| arg == "--api-key")
                .and_then(|idx| final_args.get(idx + 1).cloned())
                .or_else(|| {
                    final_args
                        .iter()
                        .find_map(|arg| arg.strip_prefix("--api-key=").map(|s| s.to_string()))
                });

            let http_client = self.http_client.clone();
            let metrics = self.metrics.clone();
            let memory_sampler = self.memory_sampler.clone();
            let needs_eviction = self.needs_eviction.clone();
            let gossip_trigger = self.gossip_trigger.clone();
            let model_key = format!("{}:{}", model_name, profile.id);
            let args_hash_clone = args_hash.clone();
            let is_cold = is_cold_start;
            tokio::spawn(async move {
                let inst = inst_clone.read().await;
                if let Err(e) = inst
                    .wait_for_ready(startup_timeout, api_key, &http_client)
                    .await
                {
                    error!("Instance {} failed to become ready: {}", inst.id, e);
                    inst.clear_startup_log();
                    let _ = inst.stop().await;
                } else {
                    // Parse model params from startup log now that instance is ready
                    inst.parse_and_store_startup_params();
                    inst.clear_startup_log();

                    // Set eviction tenure — instance cannot be drained by competitors until this expires
                    {
                        let mut evictable = inst.evictable_after.lock();
                        *evictable = Some(std::time::Instant::now() + std::time::Duration::from_secs(eviction_tenure_secs));
                    }

                    // Clear needs_eviction — this model no longer needs eviction to spawn
                    needs_eviction.write().await.remove(&model_key);

                    // Trigger instant gossip so peers learn about new model availability
                    gossip_trigger.notify_one();

                    if is_cold {
                        // Cold start: sample memory after ready and store learned value
                        if let Some(pid) = inst.get_pid() {
                            // Wait a moment for memory to stabilize after model load
                            tokio::time::sleep(std::time::Duration::from_secs(2)).await;
                            let (vram, sysmem) = memory_sampler.sample(pid);
                            if vram > 0 || sysmem > 0 {
                                let hash_metrics = metrics.get_hash_metrics(&args_hash_clone).await;
                                hash_metrics.observe_memory(vram, sysmem);
                                info!(
                                    instance_id = %inst.id,
                                    args_hash = %args_hash_clone,
                                    vram_mb = vram,
                                    sysmem_mb = sysmem,
                                    "Learned memory usage for cold start"
                                );
                            }
                        }
                    }
                }
            });

            if reserve_slot {
                let mut inst_write = inst_arc.write().await;
                inst_write.in_flight_requests = 1;
                inst_write.last_activity = std::time::Instant::now();
            }

            info!(
                event = "instance_spawn",
                model = %model_name,
                profile = %profile.id,
                instance_id = %instance_id,
                port = port,
                reason = %reason
            );
            return Ok(inst_arc.clone());
        }
    }

    pub async fn get_self_peer_state(&self) -> PeerState {
        let (curr_vram, curr_sysmem) = self.calculate_current_load().await;
        let instances = self.instances.read().await;
        let active_instances = instances.len();
        let max_instances = self.config.max_instances_per_node;

        // Collect currently loaded models and count instances per model
        let mut loaded_models = Vec::new();
        let mut instance_counts: HashMap<String, usize> = HashMap::new();
        for inst_lock in instances.values() {
            let inst = inst_lock.read().await;
            if *inst.status.lock() == InstanceStatus::Ready {
                let key = format!("{}:{}", inst.model_name, inst.profile_id);
                loaded_models.push(key.clone());
                *instance_counts.entry(key).or_default() += 1;
            }
        }
        drop(instances);

        let mut total_queue_length = 0;
        let mut model_stats = HashMap::new();
        {
            let queues = self.queues.read().await;
            for (key, queue) in queues.iter() {
                total_queue_length += queue.len();

                // Calculate TPS and memory for this model by looking up hash metrics
                let (tps, vram_mb, sysmem_mb) =
                    if let Some(args_hash) = self.get_args_hash_for_key(key).await {
                        let hash_metrics = self.metrics.get_hash_metrics(&args_hash).await;
                        (
                            hash_metrics.tokens_per_second(),
                            hash_metrics.peak_vram_mb.load(Ordering::Relaxed),
                            hash_metrics.peak_sysmem_mb.load(Ordering::Relaxed),
                        )
                    } else {
                        (0.0, 0, 0)
                    };

                model_stats.insert(
                    key.clone(),
                    PeerModelStats {
                        tps,
                        queue_len: queue.len(),
                        vram_mb,
                        sysmem_mb,
                        instance_count: instance_counts.get(key).copied().unwrap_or(0),
                    },
                );
            }
        }

        // Also include loaded models that don't have queues yet (for memory info)
        for key in loaded_models.iter() {
            if !model_stats.contains_key(key) {
                let (tps, vram_mb, sysmem_mb) =
                    if let Some(args_hash) = self.get_args_hash_for_key(key).await {
                        let hash_metrics = self.metrics.get_hash_metrics(&args_hash).await;
                        (
                            hash_metrics.tokens_per_second(),
                            hash_metrics.peak_vram_mb.load(Ordering::Relaxed),
                            hash_metrics.peak_sysmem_mb.load(Ordering::Relaxed),
                        )
                    } else {
                        (0.0, 0, 0)
                    };
                model_stats.insert(
                    key.clone(),
                    PeerModelStats {
                        tps,
                        queue_len: 0,
                        vram_mb,
                        sysmem_mb,
                        instance_count: instance_counts.get(key).copied().unwrap_or(0),
                    },
                );
            }
        }

        // Use public_url if set and non-empty, otherwise derive from listen_addr
        let address = self
            .config
            .public_url
            .as_ref()
            .filter(|url| !url.is_empty())
            .cloned()
            .unwrap_or_else(|| {
                let addr = &self.config.listen_addr;
                let scheme = if self
                    .config
                    .server_tls
                    .as_ref()
                    .map(|t| t.enabled)
                    .unwrap_or(false)
                {
                    "https"
                } else {
                    "http"
                };
                if addr.starts_with("0.0.0.0") {
                    let port = addr.split(':').nth(1).unwrap_or("8080");
                    format!("{}://127.0.0.1:{}", scheme, port)
                } else {
                    format!("{}://{}", scheme, addr)
                }
            });

        let supported_models = {
            let cookbook = self.cookbook.read().await;
            cookbook
                .models
                .iter()
                .filter(|m| m.enabled)
                .flat_map(|model| {
                    let model_defaults = &self.config.model_defaults;
                    model.profiles.iter().filter_map(move |profile| {
                        if profile.effective_max_instances(model_defaults) == 0 {
                            return None;
                        }
                        // Use bare model name for default profile
                        if profile.id == "default" {
                            Some(model.name.clone())
                        } else {
                            Some(format!("{}:{}", model.name, profile.id))
                        }
                    })
                })
                .collect()
        };

        PeerState {
            node_id: self.config.node_id.clone(),
            address,
            version: env!("CARGO_PKG_VERSION").to_string(),
            last_seen: std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0),
            supported_models,
            model_stats,
            active_instances,
            max_instances,
            current_requests: self.metrics.current_requests.load(Ordering::Relaxed),
            available_vram: self.config.max_vram_mb.saturating_sub(curr_vram),
            available_sysmem: self.config.max_sysmem_mb.saturating_sub(curr_sysmem),
            max_vram: self.config.max_vram_mb,
            max_sysmem: self.config.max_sysmem_mb,
            total_queue_length,
            ready: self.can_serve_locally(),
            loaded_models,
        }
    }

    /// Returns true if the local node can serve requests
    /// (not draining and binary exists)
    pub fn can_serve_locally(&self) -> bool {
        !self.draining.load(Ordering::Relaxed) && self.build_manager.can_serve()
    }

    /// Select the best node to handle a request.
    ///
    /// If `prefer_local` is true (e.g., the request was already forwarded from another peer),
    /// the local node is strongly preferred to avoid ping-pong routing loops.
    pub async fn select_best_node(
        &self,
        model_name: &str,
        profile: &Profile,
        prefer_local: bool,
    ) -> Option<PeerState> {
        let (_curr_vram, _curr_sysmem) = self.calculate_current_load().await;

        let mut candidates = Vec::new();
        let requested_profile = profile.id.clone();
        let requested_key = format!("{}:{}", model_name, requested_profile);

        // Add self only if we can serve requests locally
        // This excludes us when: draining or binary missing
        if self.can_serve_locally() {
            let self_peer = self.get_self_peer_state().await;
            if peer_supports_profile(&self_peer, model_name, &requested_profile) {
                // If prefer_local is set (forwarded request), return self immediately
                // This prevents ping-pong routing loops between nodes
                if prefer_local {
                    info!(
                        event = "route_decision",
                        chosen_node = %self_peer.node_id,
                        candidate_nodes = ?[&self_peer.node_id],
                        "Route decision made (prefer_local)"
                    );
                    return Some(self_peer);
                }
                candidates.push(self_peer);
            }
        }

        // Add peers
        let peers = self.peers.read().await;
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap_or_default()
            .as_secs();

        // 3x gossip interval as timeout, minimum 15s
        let timeout = self.config.cluster.gossip_interval_seconds.saturating_mul(3).max(15);

        for peer in peers.values() {
            if now.saturating_sub(peer.last_seen) > timeout {
                continue;
            }
            if peer.max_instances == 0 {
                continue;
            }
            // Skip peers that aren't ready (draining, etc.)
            if !peer.ready {
                continue;
            }
            // Skip peers with open circuit breaker (unless checking would cause await in sync context)
            // We check circuit state synchronously here using a try_read approach
            if !self.circuit_breaker.should_allow_sync(&peer.node_id) {
                tracing::debug!(
                    peer_id = %peer.node_id,
                    "Skipping peer due to open circuit breaker"
                );
                continue;
            }

            if peer_supports_profile(peer, model_name, &requested_profile) {
                candidates.push(peer.clone());
            }
        }

        // Get local learned memory estimate for this model (used when peer doesn't have stats)
        let local_memory_estimate = self.get_memory_estimate_for_key(&requested_key).await;

        // Intelligent Routing Formula:
        // Score = load_score + cold_start_cost - performance_bonus
        //
        // load_score: Penalizes busy nodes
        //   = (queue_length × 50) + (in_flight_requests × 50)
        //
        // cold_start_cost: Penalizes nodes that need to start/evict instances
        //   - Model loaded: 0
        //   - Model fits without eviction: +30 (cold start latency)
        //   - Model needs eviction: +500 + (eviction_count × 200)
        //   - Model can't fit (even with full eviction): +10000
        //
        // performance_bonus: Rewards fast nodes with headroom
        //   = min(tps, 200) × 5 + (remaining_instance_slots × 50)
        //
        // Score all candidates and sort
        let mut scored: Vec<(PeerState, f64)> = candidates
            .into_iter()
            .map(|c| {
                let score = calculate_node_score(&c, &requested_key, local_memory_estimate);
                (c, score)
            })
            .collect();
        scored.sort_by(|a, b| {
            a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal)
        });

        if let Some((chosen, _)) = scored.first() {
            let scores: Vec<String> = scored.iter().map(|(c, s)| format!("{}={:.0}", c.node_id, s)).collect();
            info!(
                event = "route_decision",
                chosen_node = %chosen.node_id,
                scores = ?scores,
                "Route decision made"
            );
        }

        scored.into_iter().next().map(|(c, _)| c)
    }

    /// Check if there are pending waiters (either in queue or notified-but-not-yet-acquired)
    /// for a given model:profile. Used to ensure fresh requests don't bypass queued waiters.
    async fn has_pending_waiters(&self, model_name: &str, profile_id: &str) -> bool {
        let key = format!("{}:{}", model_name, profile_id);

        // Check queue
        {
            let queues = self.queues.read().await;
            if let Some(queue) = queues.get(&key) {
                if !queue.is_empty() {
                    return true;
                }
            }
        }

        // Check pending tokens (waiters notified but not yet acquired slot)
        {
            let pending = self.pending_tokens.read().await;
            if let Some(set) = pending.get(&key) {
                if !set.is_empty() {
                    return true;
                }
            }
        }

        false
    }

    /// Remove a pending token after waiter successfully acquires slot or times out.
    async fn remove_pending_token(&self, model_name: &str, profile_id: &str, token: u64) {
        let key = format!("{}:{}", model_name, profile_id);
        let mut pending = self.pending_tokens.write().await;
        if let Some(set) = pending.get_mut(&key) {
            if set.remove(&token) {
                self.metrics.dec_pending_tokens();
            }
        }
    }

    /// Notify one waiter for a specific model that capacity is available.
    /// Pops waiters from the queue until one successfully receives the notification.
    /// Dropped/closed receivers are skipped. Generates a pending token for the notified waiter.
    pub async fn notify_queue(&self, model_name: &str, profile_id: &str) {
        let key = format!("{}:{}", model_name, profile_id);
        // LOCK_ORDER: queues (2) - will release before pending_tokens (3)
        let mut queues = self.queues.write().await;
        if let Some(queue) = queues.get_mut(&key) {
            while let Some(tx) = queue.pop_front() {
                // Generate unique token for this notification (atomic, no lock needed)
                let token = self.pending_token_counter.fetch_add(1, Ordering::SeqCst);

                if tx.send(token).is_ok() {
                    // Successfully sent - add token to pending set
                    // LOCK_ORDER: Must drop queues (2) before acquiring pending_tokens (3)
                    drop(queues);
                    let mut pending = self.pending_tokens.write().await;
                    pending.entry(key).or_default().insert(token);
                    self.metrics.inc_pending_tokens();
                    info!(event = "queue_dequeue", model = %model_name, token = token);
                    return;
                }
                // Receiver dropped, try next waiter (token not added to pending)
            }
        }
    }

    /// Notify one waiter per model that capacity is available.
    /// Called when overall node capacity frees up (e.g., instance crash, eviction).
    pub async fn notify_all_queues(&self) {
        // LOCK_ORDER: queues (2) - will release before pending_tokens (3)
        let mut queues = self.queues.write().await;
        let mut tokens_to_add: Vec<(String, u64)> = Vec::new();

        for (key, queue) in queues.iter_mut() {
            while let Some(tx) = queue.pop_front() {
                let token = self.pending_token_counter.fetch_add(1, Ordering::SeqCst);
                if tx.send(token).is_ok() {
                    tokens_to_add.push((key.clone(), token));
                    info!(event = "queue_dequeue", model_key = %key, token = token);
                    break; // Wake one waiter per model
                }
                // Receiver dropped, try next waiter
            }
        }
        // LOCK_ORDER: Must drop queues (2) before acquiring pending_tokens (3)
        drop(queues);

        // Add all pending tokens in one lock acquisition
        if !tokens_to_add.is_empty() {
            let count = tokens_to_add.len();
            // LOCK_ORDER: pending_tokens (3) - safe, queues already released
            let mut pending = self.pending_tokens.write().await;
            for (key, token) in tokens_to_add {
                pending.entry(key).or_default().insert(token);
            }
            // Increment metrics for each token added
            for _ in 0..count {
                self.metrics.inc_pending_tokens();
            }
        }

        // Wake any cluster-aware waiters (route_or_wait callers)
        self.capacity_notify.notify_waiters();
    }

    // ── Drain Scheduling ─────────────────────────────────────────────────

    /// Check if any queued model needs eviction of this instance's resources.
    /// Returns true if there's a model:profile in `needs_eviction` that belongs
    /// to a different model AND still has a non-empty queue.
    pub async fn has_queued_competitors_needing_eviction(&self, inst_model: &str) -> bool {
        let needs = self.needs_eviction.read().await;
        if needs.is_empty() {
            return false;
        }
        let queues = self.queues.read().await;
        needs.iter().any(|key| {
            let model = key.split(':').next().unwrap_or("");
            model != inst_model && queues.get(key).map(|q| !q.is_empty()).unwrap_or(false)
        })
    }

    /// Check if the given model has pending requests (in queue or pending tokens).
    pub async fn has_pending_for_model(&self, model: &str, profile: &str) -> bool {
        let key = format!("{}:{}", model, profile);
        let queues = self.queues.read().await;
        let has_queue = queues.get(&key).map(|q| !q.is_empty()).unwrap_or(false);
        if has_queue {
            return true;
        }
        drop(queues);
        let pending = self.pending_tokens.read().await;
        pending.get(&key).map(|s| !s.is_empty()).unwrap_or(false)
    }

    /// Cancel drains that are no longer needed (competitors were handled by
    /// peers or timed out). Notifies the drained model's queue so its
    /// pending requests can resume being dispatched.
    pub async fn maybe_cancel_drains(&self) {
        let instances = self.instances.read().await;
        for inst_lock in instances.values() {
            let inst = inst_lock.read().await;
            if inst.draining.load(std::sync::atomic::Ordering::Relaxed) {
                if !self.has_queued_competitors_needing_eviction(&inst.model_name).await {
                    inst.draining.store(false, std::sync::atomic::Ordering::Relaxed);
                    info!(
                        event = "drain_cancelled",
                        model = %inst.model_name,
                        profile = %inst.profile_id,
                        instance_id = %inst.id,
                        "Drain cancelled — no more competitors needing eviction"
                    );
                    drop(inst);
                    // Wake the model's own queue so pending requests can be dispatched
                    let model_name = inst_lock.read().await.model_name.clone();
                    let profile_id = inst_lock.read().await.profile_id.clone();
                    drop(instances);
                    self.notify_queue(&model_name, &profile_id).await;
                    return; // Released instances lock, must restart iteration
                }
            }
        }
    }

    pub async fn run_background_tasks(self: Arc<Self>) {
        info!("Starting background tasks loop");
        loop {
            tokio::time::sleep(std::time::Duration::from_secs(10)).await;

            // Memory sampling: update learned memory with peak values
            self.sample_all_instance_memory().await;

            // Eviction
            if let Err(e) = self.check_idle_instances().await {
                error!("Error in eviction loop: {}", e);
            }

            // Metrics Persistence
            let version = self.build_manager.get_version().await;
            let build_status = self.build_manager.build_status();
            let snapshot = self
                .metrics
                .snapshot(self.config.node_id.clone(), version, Some(build_status))
                .await;
            let path = std::path::Path::new(&self.config.metrics_path);
            if let Ok(json) = serde_json::to_string_pretty(&snapshot) {
                // Atomic write: write to temp file then rename
                let tmp_path = path.with_extension("tmp");
                if tokio::fs::write(&tmp_path, json).await.is_ok() {
                    if let Err(e) = tokio::fs::rename(&tmp_path, path).await {
                        tracing::warn!(
                            "Failed to rename metrics file: {}. Metrics file may be stale.",
                            e
                        );
                    }
                }
            }
        }
    }

    /// Sample memory for all running instances and update learned values.
    async fn sample_all_instance_memory(&self) {
        let instances = self.instances.read().await;
        for inst_lock in instances.values() {
            let inst = inst_lock.read().await;
            if let Some(pid) = inst.get_pid() {
                let (vram, sysmem) = self.memory_sampler.sample(pid);
                if vram > 0 || sysmem > 0 {
                    let hash_metrics = self.metrics.get_hash_metrics(&inst.args_hash).await;
                    hash_metrics.observe_memory(vram, sysmem);
                }
            }
        }
    }

    async fn check_idle_instances(&self) -> Result<()> {
        enum EvictionReason {
            Idle,
            Crashed,
            /// Instance was draining (e.g., after binary swap) and has no in-flight requests.
            Drained,
        }

        // 1. Identify victims (Read Lock)
        let instances_map = self.instances.read().await;
        let mut to_evict: Vec<(String, Arc<RwLock<Instance>>, EvictionReason)> = Vec::new();

        for (id, inst_lock) in instances_map.iter() {
            let inst = inst_lock.read().await;
            if !inst.is_alive() {
                to_evict.push((id.clone(), inst_lock.clone(), EvictionReason::Crashed));
                continue;
            }

            // Draining instances with no in-flight requests should be terminated immediately
            // (no idle timeout wait). This handles binary-swap draining.
            if inst.draining.load(std::sync::atomic::Ordering::Relaxed)
                && inst.in_flight_requests == 0
            {
                to_evict.push((id.clone(), inst_lock.clone(), EvictionReason::Drained));
                continue;
            }

            if inst.in_flight_requests == 0 {
                // Determine timeout from profile
                let mut timeout_secs = 600; // Default fallback
                if let Some((_, profile)) = self
                    .resolve_model(&format!("{}:{}", inst.model_name, inst.profile_id))
                    .await
                {
                    timeout_secs = profile.idle_timeout_seconds;
                }

                if inst.last_activity.elapsed().as_secs() > timeout_secs {
                    to_evict.push((id.clone(), inst_lock.clone(), EvictionReason::Idle));
                }
            }
        }
        drop(instances_map);

        if to_evict.is_empty() {
            return Ok(());
        }

        // 2. Remove and Stop (Write Lock + Non-blocking Stop)
        let mut instances_map = self.instances.write().await;
        let mut confirmed_victims = Vec::new();

        for (id, inst_lock, reason) in to_evict {
            // Re-verify status under write lock to avoid race with new requests
            if let Some(current_lock) = instances_map.get(&id) {
                // We must check if it is still idle.
                // It is safe to acquire inner read lock while holding outer write lock.
                let inst = current_lock.read().await;
                let still_eligible = match reason {
                    EvictionReason::Crashed => true,
                    EvictionReason::Idle => inst.in_flight_requests == 0,
                    EvictionReason::Drained => {
                        inst.draining.load(std::sync::atomic::Ordering::Relaxed)
                            && inst.in_flight_requests == 0
                    }
                };

                if still_eligible {
                    confirmed_victims.push((id.clone(), inst_lock.clone(), reason));
                } else {
                    info!(
                        "Instance {} became active or recovered during eviction check; skipping",
                        id
                    );
                }
            }
        }

        // Collect ports BEFORE dropping the instances lock to prevent race
        let mut ports_to_release: Vec<u16> = Vec::new();
        for (_, inst_lock, _) in &confirmed_victims {
            let inst = inst_lock.read().await;
            ports_to_release.push(inst.port);
        }

        for (id, _, _) in &confirmed_victims {
            instances_map.remove(id);
        }

        // Release ports while still holding instances lock
        for port in &ports_to_release {
            self.release_port(*port).await;
        }
        drop(instances_map);

        let any_evicted = !confirmed_victims.is_empty();

        for (_, inst_lock, reason) in confirmed_victims {
            let inst = inst_lock.read().await;
            let reason_str = match reason {
                EvictionReason::Idle => "idle",
                EvictionReason::Crashed => "error",
                EvictionReason::Drained => "drained",
            };
            self.stop_and_cleanup_instance(&inst, reason_str).await;
        }

        if any_evicted {
            self.notify_all_queues().await;
        }

        Ok(())
    }

    pub async fn shutdown_all_instances(&self) {
        info!("Shutting down all instances...");
        let mut instances_map = self.instances.write().await;
        let mut instances = Vec::new();
        for (id, inst_lock) in instances_map.drain() {
            instances.push((id, inst_lock));
        }
        drop(instances_map);

        for (_, inst_lock) in instances {
            let inst = inst_lock.read().await;
            self.stop_and_cleanup_instance(&inst, "manual").await;
        }
        info!("All instances shut down.");
    }

    pub async fn is_idle(&self) -> bool {
        // Check current requests
        if self.metrics.current_requests.load(Ordering::Relaxed) > 0 {
            return false;
        }

        // Check queues
        let queues = self.queues.read().await;
        for queue in queues.values() {
            if !queue.is_empty() {
                return false;
            }
        }
        true
    }
}

fn peer_supports_profile(peer: &PeerState, model_name: &str, profile_id: &str) -> bool {
    let requested = format!("{}:{}", model_name, profile_id);
    peer.supported_models.iter().any(|entry| {
        entry.eq_ignore_ascii_case(&requested)
            || (entry.eq_ignore_ascii_case(model_name) && profile_id == "default")
    })
}

/// Calculate routing score for a node. Lower score = better choice.
///
/// Score formula:
///   score = load_score + cold_start_cost - performance_bonus
///
/// Where:
/// - load_score = (queue_length × 50) + (in_flight_requests × 50)
/// - cold_start_cost depends on whether model is loaded and memory availability
/// - performance_bonus = min(tps, 200) × 5 + (remaining_slots × 50)
fn calculate_node_score(
    peer: &PeerState,
    model_key: &str,
    local_memory_estimate: (u64, u64),
) -> f64 {
    let stats = peer.model_stats.get(model_key);

    // Extract model statistics (TPS, memory requirements, instance count)
    let tps = stats.map(|s| s.tps).unwrap_or(0.0);
    let (vram_required, sysmem_required) = stats
        .map(|s| (s.vram_mb, s.sysmem_mb))
        .filter(|(v, s)| *v > 0 || *s > 0)
        .unwrap_or(local_memory_estimate);
    let model_instance_count = stats.map(|s| s.instance_count).unwrap_or(0);

    // Check if model is already loaded
    let model_loaded = peer.loaded_models.iter().any(|m| m == model_key);

    // Calculate base load score (penalizes busy nodes)
    // Per-request penalty increased from 10 to 50 so that 1 busy instance
    // on a warm node makes cold-starting on an idle node more attractive.
    let load_score = (peer.total_queue_length as f64 * 50.0) + (peer.current_requests as f64 * 50.0);

    // Calculate cold start cost
    let cold_start_cost = if model_loaded {
        // Model already loaded - no cold start cost
        // But check if we have room for more instances (for load balancing)
        if model_instance_count > 0 {
            0.0
        } else {
            // Model is in loaded_models but instance_count is 0 (shouldn't happen, but handle it)
            0.0
        }
    } else {
        // Model not loaded - need to spawn a new instance
        calculate_cold_start_cost(peer, vram_required, sysmem_required)
    };

    // Calculate performance bonus
    // TPS bonus: capped at 1000 (200 TPS × 5)
    let tps_bonus = tps.min(200.0) * 5.0;

    // Headroom bonus: reward nodes with room for more instances
    let remaining_slots = peer.max_instances.saturating_sub(peer.active_instances);
    let headroom_bonus = (remaining_slots as f64).min(4.0) * 50.0; // Cap at 200 (4 slots × 50)

    // Final score (lower is better)
    load_score + cold_start_cost - tps_bonus - headroom_bonus
}

/// Calculate the cold start cost for spawning a model on a node.
///
/// Returns:
/// - 30 if model fits without eviction (just cold start latency)
/// - 500 + (eviction_count × 200) if eviction is needed
/// - 10000 if model can't fit even with full eviction
fn calculate_cold_start_cost(peer: &PeerState, vram_required: u64, sysmem_required: u64) -> f64 {
    // Cold start penalty reduced from 200 to 30 to favor spawning on idle nodes
    // over queueing behind busy instances on warm nodes.
    const COLD_START_PENALTY: f64 = 30.0;
    const EVICTION_BASE_PENALTY: f64 = 500.0;
    const EVICTION_PER_INSTANCE: f64 = 200.0;
    const CANNOT_FIT_PENALTY: f64 = 10000.0;

    // If memory requirements are unknown (0), assume model fits (optimistic)
    if vram_required == 0 && sysmem_required == 0 {
        return COLD_START_PENALTY;
    }

    // Check if model fits with current available memory
    let fits_vram = vram_required == 0 || peer.available_vram >= vram_required;
    let fits_sysmem = sysmem_required == 0 || peer.available_sysmem >= sysmem_required;

    if fits_vram && fits_sysmem {
        // Model fits without eviction
        return COLD_START_PENALTY;
    }

    // Model doesn't fit - estimate eviction cost
    // We need to estimate how many instances would need to be evicted

    // If max capacity isn't enough, model can't fit
    let total_vram_ok = vram_required == 0 || peer.max_vram >= vram_required;
    let total_sysmem_ok = sysmem_required == 0 || peer.max_sysmem >= sysmem_required;

    if !total_vram_ok || !total_sysmem_ok {
        // Model can never fit on this node
        return CANNOT_FIT_PENALTY;
    }

    // Estimate eviction count based on memory deficit
    let vram_deficit = vram_required.saturating_sub(peer.available_vram);
    let sysmem_deficit = sysmem_required.saturating_sub(peer.available_sysmem);

    // Rough estimate: assume each instance uses avg memory
    // avg_vram_per_instance = (max_vram - available_vram) / active_instances
    let eviction_count = if peer.active_instances == 0 {
        // No instances to evict, but somehow memory is insufficient
        // This shouldn't happen, but handle it conservatively
        1
    } else {
        let used_vram = peer.max_vram.saturating_sub(peer.available_vram);
        let used_sysmem = peer.max_sysmem.saturating_sub(peer.available_sysmem);

        let avg_vram = if used_vram > 0 {
            used_vram / peer.active_instances as u64
        } else {
            1 // Avoid division issues
        };
        let avg_sysmem = if used_sysmem > 0 {
            used_sysmem / peer.active_instances as u64
        } else {
            1
        };

        // How many instances to evict to free enough memory?
        let vram_evictions = if avg_vram > 0 && vram_deficit > 0 {
            vram_deficit.div_ceil(avg_vram)
        } else {
            0
        };
        let sysmem_evictions = if avg_sysmem > 0 && sysmem_deficit > 0 {
            sysmem_deficit.div_ceil(avg_sysmem)
        } else {
            0
        };

        vram_evictions.max(sysmem_evictions).max(1) as usize
    };

    // Cap eviction count at active_instances
    let eviction_count = eviction_count.min(peer.active_instances);

    EVICTION_BASE_PENALTY + (eviction_count as f64 * EVICTION_PER_INSTANCE)
}

/// Get effective queue timeout. Returns None for infinite wait (when configured value is 0).
fn effective_queue_timeout(profile: &Profile, defaults: &ModelDefaults) -> Option<Duration> {
    let ms = profile
        .max_wait_in_queue_ms
        .unwrap_or(defaults.max_wait_in_queue_ms);
    if ms == 0 {
        None // 0 = infinite wait
    } else {
        Some(Duration::from_millis(ms))
    }
}

/// Legacy helper for tests - returns ms value (1 if infinite)
#[cfg(test)]
fn effective_queue_timeout_ms(profile: &Profile, defaults: &ModelDefaults) -> u64 {
    effective_queue_timeout(profile, defaults)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(1) // For test compatibility
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build_manager::BuildManager;
    use crate::config::{ClusterConfig, HttpConfig, LlamaCppConfig, Model, ModelDefaults};

    fn sample_peer(supported: Vec<&str>) -> PeerState {
        PeerState {
            node_id: "peer-a".into(),
            address: "http://peer-a".into(),
            version: "0.1.0".into(),
            last_seen: 0,
            supported_models: supported.into_iter().map(|s| s.to_string()).collect(),
            active_instances: 0,
            max_instances: 8,
            current_requests: 0,
            available_vram: 1,
            available_sysmem: 1,
            max_vram: 16000,
            max_sysmem: 64000,
            total_queue_length: 0,
            model_stats: HashMap::new(),
            ready: true,
            loaded_models: vec![],
        }
    }

    #[test]
    fn peer_matches_explicit_profile_entry() {
        let peer = sample_peer(vec!["gpt:fast"]);
        assert!(peer_supports_profile(&peer, "gpt", "fast"));
        assert!(!peer_supports_profile(&peer, "gpt", "default"));
    }

    #[test]
    fn peer_matches_default_profile_with_base_name() {
        let peer = sample_peer(vec!["gpt"]);
        assert!(peer_supports_profile(&peer, "gpt", "default"));
        assert!(!peer_supports_profile(&peer, "gpt", "quality"));
    }

    #[test]
    fn peer_rejects_missing_profile() {
        let peer = sample_peer(vec!["other:fast"]);
        assert!(!peer_supports_profile(&peer, "gpt", "fast"));
    }

    fn sample_profile() -> Profile {
        Profile {
            id: "default".into(),
            description: None,
            model_path: Some("/tmp/model.gguf".into()),
            hf_repo: None,
            hf_file: None,
            idle_timeout_seconds: 0,
            max_instances: Some(1),
            llama_server_args: Vec::new(),
            max_wait_in_queue_ms: None,
            max_request_duration_ms: None,
            startup_timeout_seconds: None,
            download_timeout_seconds: None,
            max_queue_size: None,
            min_eviction_tenure_secs: None,
        }
    }

    fn sample_defaults() -> ModelDefaults {
        ModelDefaults {
            max_concurrent_requests_per_instance: 1,
            max_queue_size_per_model: 1,
            max_instances_per_model: 1,
            max_wait_in_queue_ms: 500,
            max_request_duration_ms: 300_000,
            min_eviction_tenure_secs: 15,
        }
    }

    #[test]
    fn queue_timeout_uses_profile_override() {
        let mut profile = sample_profile();
        profile.max_wait_in_queue_ms = Some(1234);
        let defaults = sample_defaults();

        assert_eq!(effective_queue_timeout_ms(&profile, &defaults), 1234);
    }

    #[test]
    fn queue_timeout_falls_back_to_defaults() {
        let profile = sample_profile();
        let mut defaults = sample_defaults();
        defaults.max_wait_in_queue_ms = 42;

        assert_eq!(effective_queue_timeout_ms(&profile, &defaults), 42);
    }

    #[test]
    fn queue_timeout_clamps_to_minimum_millisecond() {
        let profile = sample_profile();
        let mut defaults = sample_defaults();
        defaults.max_wait_in_queue_ms = 0;

        assert_eq!(effective_queue_timeout_ms(&profile, &defaults), 1);
    }

    // --- Integration-like test for dynamic peer discovery ---

    fn minimal_node_config() -> NodeConfig {
        NodeConfig {
            node_id: "node-local".to_string(),
            listen_addr: "0.0.0.0:8080".to_string(),
            public_url: None,
            max_vram_mb: 1024,
            max_sysmem_mb: 1024,
            max_instances_per_node: 10,
            metrics_path: "./metrics.json".to_string(),
            default_model: "test".to_string(),
            model_defaults: sample_defaults(),
            llama_cpp_ports: None,
            llama_cpp: LlamaCppConfig {
                repo_url: "https://github.com/ggml-org/llama.cpp.git".into(),
                repo_path: "./llama.cpp".into(),
                build_path: "./llama.cpp/build".into(),
                binary_path: "./llama.cpp/bin/llama-server".into(),
                branch: "master".into(),
                build_args: vec![],
                build_command_args: vec![],
                auto_update_interval_seconds: 0,
                enabled: false,
                keep_builds: 3,
            },
            cluster: ClusterConfig {
                enabled: true,
                peers: vec![], // Start with NO peers
                gossip_interval_seconds: 5,
                max_concurrent_gossip: 16,
                discovery: Default::default(),
                noise: Default::default(),
                circuit_breaker: Default::default(),
                version_mismatch_action: "warn".to_string(),
            },
            http: HttpConfig {
                request_body_limit_bytes: 1024,
                idle_timeout_seconds: 60,
                body_read_timeout_ms: 30_000,
                protocol_detect_timeout_ms: 10_000,
            },
            auth: None,
            server_tls: None,
            cluster_tls: None,
            shutdown_grace_period_seconds: 30,
            max_hops: 10,
            logging: None,
            max_total_queue_entries: 0,
        }
    }

    #[tokio::test]
    async fn select_best_node_picks_dynamic_peer() {
        let config = minimal_node_config();
        let cookbook = Cookbook { models: vec![] };
        let build_manager = BuildManager::new(config.llama_cpp.clone());

        let state = NodeState::new(config, cookbook, build_manager)
            .await
            .unwrap();

        // 1. Initially no peers, and local cookbook is empty.
        let profile = sample_profile();
        let result = state
            .select_best_node("remote-model", &profile, false)
            .await;
        // Should be None because we don't support it and we have no peers.
        assert!(result.is_none());

        // 2. Simulate discovering a peer via gossip
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();

        let peer = PeerState {
            node_id: "remote-peer".into(),
            address: "http://remote-peer".into(),
            version: "0.1.0".into(),
            last_seen: now,                                        // Recent
            supported_models: vec!["remote-model:default".into()], // Supports our model
            active_instances: 0,
            max_instances: 10,
            current_requests: 0,
            available_vram: 10000,
            available_sysmem: 10000,
            max_vram: 16000,
            max_sysmem: 64000,
            total_queue_length: 0,
            model_stats: HashMap::new(),
            ready: true,
            loaded_models: vec![],
        };

        {
            let mut peers_lock = state.peers.write().await;
            peers_lock.insert(peer.node_id.clone(), peer);
        }

        // 3. Now select_best_node should find the peer
        let result = state
            .select_best_node("remote-model", &profile, false)
            .await;
        assert!(result.is_some());
        let picked = result.unwrap();
        assert_eq!(picked.node_id, "remote-peer");
    }

    #[tokio::test]
    async fn pick_victims_evicts_lru() {
        // Setup
        let config = minimal_node_config();
        let profile_small = sample_profile();
        let model_small = Model {
            name: "small".into(),
            description: None,
            enabled: true,
            profiles: vec![profile_small.clone()],
        };
        let cookbook = Cookbook {
            models: vec![model_small],
        };
        let build_manager = BuildManager::new(config.llama_cpp.clone());
        let state = NodeState::new(config, cookbook, build_manager)
            .await
            .unwrap();

        // Set up learned memory for the test args_hash (100MB VRAM each)
        let test_args_hash = "test_args_hash".to_string();
        let hash_metrics = state.metrics.get_hash_metrics(&test_args_hash).await;
        hash_metrics.observe_memory(100, 0);

        // Manually populate instances
        // Inst A: Last activity 1000s ago (Oldest)
        let mut inst_a = Instance::new(
            "inst-a".into(),
            "small".into(),
            "default".into(),
            "host".into(),
            8000,
            test_args_hash.clone(),
            false,
        );
        inst_a.last_activity = std::time::Instant::now() - std::time::Duration::from_secs(1000);
        inst_a.in_flight_requests = 0; // Idle

        // Inst B: Last activity 10s ago (Newest)
        let mut inst_b = Instance::new(
            "inst-b".into(),
            "small".into(),
            "default".into(),
            "host".into(),
            8001,
            test_args_hash.clone(),
            false,
        );
        inst_b.last_activity = std::time::Instant::now() - std::time::Duration::from_secs(10);
        inst_b.in_flight_requests = 0; // Idle

        // Inst C: Active (Cannot be evicted)
        let mut inst_c = Instance::new(
            "inst-c".into(),
            "small".into(),
            "default".into(),
            "host".into(),
            8002,
            test_args_hash.clone(),
            false,
        );
        inst_c.last_activity = std::time::Instant::now() - std::time::Duration::from_secs(10000);
        inst_c.in_flight_requests = 1;

        {
            let mut instances = state.instances.write().await;
            instances.insert("inst-a".into(), Arc::new(RwLock::new(inst_a)));
            instances.insert("inst-b".into(), Arc::new(RwLock::new(inst_b)));
            instances.insert("inst-c".into(), Arc::new(RwLock::new(inst_c)));
        }

        // Max VRAM is 1024.
        // Current Usage: 3 * 100 = 300 (from learned memory).
        // Remaining: 724.

        // Case 1: Request 100MB. 300+100 <= 1024. No eviction needed.
        let map = state.instances.read().await;
        let victims = state.pick_victims(&map, 100, 0).await.unwrap();
        assert!(victims.is_empty());

        // Case 2: Request 900MB. 300+900 = 1200 > 1024. Need to free 176MB.
        // Removing 1 instance (100MB) -> 200+900 = 1100 > 1024. Still not enough.
        // Removing 2 instances (200MB) -> 100+900 = 1000 <= 1024. OK.
        // Victims should be [inst-a, inst-b] in order? Or just the set.
        // pick_victims returns candidates sorted by LRU.
        // inst-a is oldest (1000s ago). inst-b is next (10s ago). inst-c is active (ignored).

        // Note: pick_victims returns sufficient victims to satisfy requirement.
        let victims = state.pick_victims(&map, 900, 0).await.unwrap();
        assert_eq!(victims.len(), 2);
        assert_eq!(victims[0], "inst-a");
        assert_eq!(victims[1], "inst-b");
    }

    #[tokio::test]
    async fn test_get_available_port_skips_occupied() {
        let mut config = minimal_node_config();
        // Use a safe range for this test
        config.llama_cpp_ports = Some(crate::config::LlamaCppPorts {
            ports: None,
            ranges: Some(vec![crate::config::PortRange {
                start: 30000,
                end: 30005,
            }]),
        });

        let cookbook = Cookbook { models: vec![] };
        let build_manager = BuildManager::new(config.llama_cpp.clone());
        let state = NodeState::new(config, cookbook, build_manager)
            .await
            .unwrap();

        // Bind port 30000 manually
        let port = 30000;
        let _listener = tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .expect("Failed to bind test port");

        // The first port in range is 30000. It should be skipped.
        let picked = state.get_available_port().await.unwrap();
        assert_ne!(picked, port);
        assert_eq!(picked, 30001);

        // Verify port_pool reservations
        assert!(state.port_pool.is_reserved(30001).await);
        assert!(!state.port_pool.is_reserved(30000).await);
    }

    #[tokio::test]
    async fn test_is_idle_with_no_instances() {
        let config = minimal_node_config();
        let cookbook = Cookbook { models: vec![] };
        let build_manager = BuildManager::new(config.llama_cpp.clone());
        let state = NodeState::new(config, cookbook, build_manager)
            .await
            .unwrap();

        // No instances, no requests - should be idle
        assert!(state.is_idle().await);
    }

    #[tokio::test]
    async fn test_calculate_current_load_empty() {
        let config = minimal_node_config();
        let cookbook = Cookbook { models: vec![] };
        let build_manager = BuildManager::new(config.llama_cpp.clone());
        let state = NodeState::new(config, cookbook, build_manager)
            .await
            .unwrap();

        // No instances - load should be zero
        let load = state.calculate_current_load().await;
        assert_eq!(load.0, 0); // vram
        assert_eq!(load.1, 0); // sysmem
    }

    #[tokio::test]
    async fn test_effective_queue_timeout_default() {
        // Test with None profile timeout - should use model defaults
        let defaults = sample_defaults();
        let profile = sample_profile();
        let timeout = effective_queue_timeout_ms(&profile, &defaults);
        assert_eq!(timeout, defaults.max_wait_in_queue_ms);
    }

    #[tokio::test]
    async fn test_effective_queue_timeout_override() {
        let defaults = sample_defaults();
        let mut profile = sample_profile();
        profile.max_wait_in_queue_ms = Some(999);
        let timeout = effective_queue_timeout_ms(&profile, &defaults);
        assert_eq!(timeout, 999);
    }

    #[tokio::test]
    async fn test_evict_all_idle_instances() {
        let config = minimal_node_config();
        let cookbook = Cookbook { models: vec![] };
        let build_manager = BuildManager::new(config.llama_cpp.clone());
        let state = NodeState::new(config, cookbook, build_manager)
            .await
            .unwrap();

        // Add some instances - 2 idle, 1 active
        let test_args_hash = "test_args_hash".to_string();
        let hash_metrics = state.metrics.get_hash_metrics(&test_args_hash).await;
        hash_metrics.observe_memory(100, 0);

        let mut inst_a = Instance::new(
            "inst-a".into(),
            "model".into(),
            "default".into(),
            "host".into(),
            8000,
            test_args_hash.clone(),
            false,
        );
        inst_a.in_flight_requests = 0; // Idle

        let mut inst_b = Instance::new(
            "inst-b".into(),
            "model".into(),
            "default".into(),
            "host".into(),
            8001,
            test_args_hash.clone(),
            false,
        );
        inst_b.in_flight_requests = 0; // Idle

        let mut inst_c = Instance::new(
            "inst-c".into(),
            "model".into(),
            "default".into(),
            "host".into(),
            8002,
            test_args_hash.clone(),
            false,
        );
        inst_c.in_flight_requests = 1; // Active

        {
            let mut instances = state.instances.write().await;
            instances.insert("inst-a".into(), Arc::new(RwLock::new(inst_a)));
            instances.insert("inst-b".into(), Arc::new(RwLock::new(inst_b)));
            instances.insert("inst-c".into(), Arc::new(RwLock::new(inst_c)));
        }

        // Should have 3 instances
        assert_eq!(state.instances.read().await.len(), 3);

        // Evict all idle instances
        let evicted = state.evict_all_idle_instances().await;
        assert_eq!(evicted, 2); // Should evict the 2 idle ones

        // Should have 1 instance left (the active one)
        assert_eq!(state.instances.read().await.len(), 1);
        assert!(state.instances.read().await.contains_key("inst-c"));
    }

    #[tokio::test]
    async fn test_evict_all_idle_instances_none_idle() {
        let config = minimal_node_config();
        let cookbook = Cookbook { models: vec![] };
        let build_manager = BuildManager::new(config.llama_cpp.clone());
        let state = NodeState::new(config, cookbook, build_manager)
            .await
            .unwrap();

        // Add active instance only
        let test_args_hash = "test_args_hash".to_string();

        let mut inst_a = Instance::new(
            "inst-a".into(),
            "model".into(),
            "default".into(),
            "host".into(),
            8000,
            test_args_hash.clone(),
            false,
        );
        inst_a.in_flight_requests = 1; // Active

        {
            let mut instances = state.instances.write().await;
            instances.insert("inst-a".into(), Arc::new(RwLock::new(inst_a)));
        }

        // Evict all idle - should return 0 since none are idle
        let evicted = state.evict_all_idle_instances().await;
        assert_eq!(evicted, 0);

        // Instance should still be there
        assert_eq!(state.instances.read().await.len(), 1);
    }

    #[tokio::test]
    async fn test_instance_cold_start_flag() {
        // Test that is_cold_start flag is correctly set
        let inst = Instance::new(
            "test".into(),
            "model".into(),
            "default".into(),
            "host".into(),
            8000,
            "hash".into(),
            true, // cold start
        );
        assert!(inst.is_cold_start);

        let inst2 = Instance::new(
            "test2".into(),
            "model".into(),
            "default".into(),
            "host".into(),
            8001,
            "hash".into(),
            false, // warm start
        );
        assert!(!inst2.is_cold_start);
    }

    #[tokio::test]
    async fn test_instance_draining_flag() {
        let inst = Instance::new(
            "test".into(),
            "model".into(),
            "default".into(),
            "host".into(),
            8000,
            "hash".into(),
            false,
        );

        // Should not be draining initially
        assert!(!inst.draining.load(std::sync::atomic::Ordering::Relaxed));

        // Set draining
        inst.draining
            .store(true, std::sync::atomic::Ordering::Relaxed);
        assert!(inst.draining.load(std::sync::atomic::Ordering::Relaxed));
    }

    // ========== Scoring Function Tests ==========

    #[test]
    fn test_cold_start_cost_model_fits_without_eviction() {
        let peer = PeerState {
            node_id: "test".into(),
            address: "http://test".into(),
            version: "0.1.0".into(),
            last_seen: 0,
            supported_models: vec![],
            active_instances: 2,
            max_instances: 8,
            current_requests: 0,
            available_vram: 8000,
            available_sysmem: 16000,
            max_vram: 16000,
            max_sysmem: 64000,
            total_queue_length: 0,
            model_stats: HashMap::new(),
            ready: true,
            loaded_models: vec![],
        };

        // Model fits easily
        let cost = calculate_cold_start_cost(&peer, 4000, 8000);
        assert!((cost - 30.0).abs() < 0.1, "Expected 30 for cold start, got {}", cost);
    }

    #[test]
    fn test_cold_start_cost_unknown_memory() {
        let peer = PeerState {
            node_id: "test".into(),
            address: "http://test".into(),
            version: "0.1.0".into(),
            last_seen: 0,
            supported_models: vec![],
            active_instances: 0,
            max_instances: 8,
            current_requests: 0,
            available_vram: 0,
            available_sysmem: 0,
            max_vram: 16000,
            max_sysmem: 64000,
            total_queue_length: 0,
            model_stats: HashMap::new(),
            ready: true,
            loaded_models: vec![],
        };

        // Unknown memory (0, 0) should be optimistic (just cold start)
        let cost = calculate_cold_start_cost(&peer, 0, 0);
        assert!((cost - 30.0).abs() < 0.1, "Expected 30 for unknown memory, got {}", cost);
    }

    #[test]
    fn test_cold_start_cost_needs_eviction() {
        let peer = PeerState {
            node_id: "test".into(),
            address: "http://test".into(),
            version: "0.1.0".into(),
            last_seen: 0,
            supported_models: vec![],
            active_instances: 2,
            max_instances: 8,
            current_requests: 0,
            available_vram: 2000,   // Only 2GB available
            available_sysmem: 16000,
            max_vram: 16000,        // 16GB total
            max_sysmem: 64000,
            total_queue_length: 0,
            model_stats: HashMap::new(),
            ready: true,
            loaded_models: vec![],
        };

        // Model needs 8GB, only 2GB available - needs eviction
        let cost = calculate_cold_start_cost(&peer, 8000, 0);
        // Should be: 500 (base) + N * 200 (per instance)
        assert!(cost >= 500.0, "Expected eviction penalty >= 500, got {}", cost);
        assert!(cost < 10000.0, "Should not be cannot-fit penalty");
    }

    #[test]
    fn test_cold_start_cost_model_cannot_fit() {
        let peer = PeerState {
            node_id: "test".into(),
            address: "http://test".into(),
            version: "0.1.0".into(),
            last_seen: 0,
            supported_models: vec![],
            active_instances: 2,
            max_instances: 8,
            current_requests: 0,
            available_vram: 8000,
            available_sysmem: 16000,
            max_vram: 16000,
            max_sysmem: 64000,
            total_queue_length: 0,
            model_stats: HashMap::new(),
            ready: true,
            loaded_models: vec![],
        };

        // Model needs more VRAM than node has total
        let cost = calculate_cold_start_cost(&peer, 32000, 0);
        assert!((cost - 10000.0).abs() < 0.1, "Expected 10000 for cannot-fit, got {}", cost);
    }

    #[test]
    fn test_node_score_prefers_loaded_model_when_idle() {
        let model_key = "gpt:fast";

        // Node with model loaded and IDLE
        let mut loaded_idle_peer = PeerState {
            node_id: "loaded".into(),
            address: "http://loaded".into(),
            version: "0.1.0".into(),
            last_seen: 0,
            supported_models: vec!["gpt:fast".into()],
            active_instances: 1,
            max_instances: 8,
            current_requests: 0,  // Idle
            available_vram: 8000,
            available_sysmem: 32000,
            max_vram: 16000,
            max_sysmem: 64000,
            total_queue_length: 0,  // No queue
            model_stats: HashMap::new(),
            ready: true,
            loaded_models: vec!["gpt:fast".into()],
        };
        loaded_idle_peer.model_stats.insert(
            "gpt:fast".into(),
            PeerModelStats {
                tps: 50.0,
                queue_len: 0,
                vram_mb: 8000,
                sysmem_mb: 16000,
                instance_count: 1,
            },
        );

        // Node without model loaded (but can fit it)
        let unloaded_peer = PeerState {
            node_id: "unloaded".into(),
            address: "http://unloaded".into(),
            version: "0.1.0".into(),
            last_seen: 0,
            supported_models: vec!["gpt:fast".into()],
            active_instances: 0,
            max_instances: 8,
            current_requests: 0,
            available_vram: 16000,
            available_sysmem: 64000,
            max_vram: 16000,
            max_sysmem: 64000,
            total_queue_length: 0,
            model_stats: HashMap::new(),
            ready: true,
            loaded_models: vec![],
        };

        let local_mem = (8000, 16000);
        let loaded_score = calculate_node_score(&loaded_idle_peer, model_key, local_mem);
        let unloaded_score = calculate_node_score(&unloaded_peer, model_key, local_mem);

        // IDLE loaded node should still win (TPS bonus + no cold start)
        assert!(
            loaded_score < unloaded_score,
            "Idle loaded node ({}) should score better than idle unloaded ({})",
            loaded_score,
            unloaded_score
        );
    }

    #[test]
    fn test_node_score_prefers_unloaded_when_loaded_is_busy() {
        let model_key = "gpt:fast";

        // Node with model loaded but BUSY
        let mut loaded_busy_peer = PeerState {
            node_id: "loaded".into(),
            address: "http://loaded".into(),
            version: "0.1.0".into(),
            last_seen: 0,
            supported_models: vec!["gpt:fast".into()],
            active_instances: 1,
            max_instances: 8,
            current_requests: 5,  // Busy!
            available_vram: 8000,
            available_sysmem: 32000,
            max_vram: 16000,
            max_sysmem: 64000,
            total_queue_length: 2,  // Queue building
            model_stats: HashMap::new(),
            ready: true,
            loaded_models: vec!["gpt:fast".into()],
        };
        loaded_busy_peer.model_stats.insert(
            "gpt:fast".into(),
            PeerModelStats {
                tps: 50.0,
                queue_len: 2,
                vram_mb: 8000,
                sysmem_mb: 16000,
                instance_count: 1,
            },
        );

        // Node without model loaded (but idle and can fit it)
        let unloaded_peer = PeerState {
            node_id: "unloaded".into(),
            address: "http://unloaded".into(),
            version: "0.1.0".into(),
            last_seen: 0,
            supported_models: vec!["gpt:fast".into()],
            active_instances: 0,
            max_instances: 8,
            current_requests: 0,  // Idle
            available_vram: 16000,
            available_sysmem: 64000,
            max_vram: 16000,
            max_sysmem: 64000,
            total_queue_length: 0,  // No queue
            model_stats: HashMap::new(),
            ready: true,
            loaded_models: vec![],
        };

        let local_mem = (8000, 16000);
        let loaded_score = calculate_node_score(&loaded_busy_peer, model_key, local_mem);
        let unloaded_score = calculate_node_score(&unloaded_peer, model_key, local_mem);

        // Busy loaded node should LOSE to idle unloaded node
        // Cold-starting on idle node is better than queuing behind busy instance
        assert!(
            unloaded_score < loaded_score,
            "Idle unloaded node ({}) should score better than busy loaded ({})",
            unloaded_score,
            loaded_score
        );
    }

    #[test]
    fn test_node_score_penalizes_busy_nodes() {
        let model_key = "gpt:fast";
        let local_mem = (8000, 16000);

        // Idle node
        let idle_peer = PeerState {
            node_id: "idle".into(),
            address: "http://idle".into(),
            version: "0.1.0".into(),
            last_seen: 0,
            supported_models: vec!["gpt:fast".into()],
            active_instances: 1,
            max_instances: 8,
            current_requests: 0,
            available_vram: 8000,
            available_sysmem: 32000,
            max_vram: 16000,
            max_sysmem: 64000,
            total_queue_length: 0,
            model_stats: HashMap::new(),
            ready: true,
            loaded_models: vec!["gpt:fast".into()],
        };

        // Busy node (same model loaded)
        let busy_peer = PeerState {
            node_id: "busy".into(),
            address: "http://busy".into(),
            version: "0.1.0".into(),
            last_seen: 0,
            supported_models: vec!["gpt:fast".into()],
            active_instances: 1,
            max_instances: 8,
            current_requests: 10,   // Many in-flight
            available_vram: 8000,
            available_sysmem: 32000,
            max_vram: 16000,
            max_sysmem: 64000,
            total_queue_length: 5,  // Queued requests
            model_stats: HashMap::new(),
            ready: true,
            loaded_models: vec!["gpt:fast".into()],
        };

        let idle_score = calculate_node_score(&idle_peer, model_key, local_mem);
        let busy_score = calculate_node_score(&busy_peer, model_key, local_mem);

        // Idle should score better (lower)
        assert!(
            idle_score < busy_score,
            "Idle node ({}) should score better than busy ({})",
            idle_score,
            busy_score
        );
    }

    #[test]
    fn test_node_score_rewards_high_tps() {
        let model_key = "gpt:fast";
        let local_mem = (8000, 16000);

        // Slow node
        let mut slow_peer = PeerState {
            node_id: "slow".into(),
            address: "http://slow".into(),
            version: "0.1.0".into(),
            last_seen: 0,
            supported_models: vec!["gpt:fast".into()],
            active_instances: 1,
            max_instances: 8,
            current_requests: 0,
            available_vram: 8000,
            available_sysmem: 32000,
            max_vram: 16000,
            max_sysmem: 64000,
            total_queue_length: 0,
            model_stats: HashMap::new(),
            ready: true,
            loaded_models: vec!["gpt:fast".into()],
        };
        slow_peer.model_stats.insert(
            "gpt:fast".into(),
            PeerModelStats {
                tps: 10.0,
                queue_len: 0,
                vram_mb: 8000,
                sysmem_mb: 16000,
                instance_count: 1,
            },
        );

        // Fast node (same setup, higher TPS)
        let mut fast_peer = slow_peer.clone();
        fast_peer.node_id = "fast".into();
        fast_peer.model_stats.insert(
            "gpt:fast".into(),
            PeerModelStats {
                tps: 100.0,
                queue_len: 0,
                vram_mb: 8000,
                sysmem_mb: 16000,
                instance_count: 1,
            },
        );

        let slow_score = calculate_node_score(&slow_peer, model_key, local_mem);
        let fast_score = calculate_node_score(&fast_peer, model_key, local_mem);

        // Fast should score better (lower)
        assert!(
            fast_score < slow_score,
            "Fast node ({}) should score better than slow ({})",
            fast_score,
            slow_score
        );
    }
}
