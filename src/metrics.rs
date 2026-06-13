use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use tokio::sync::RwLock;

/// Unified metrics for a specific set of llama-server launch args (identified by hash).
#[derive(Debug)]
pub struct HashMetrics {
    // Performance metrics
    pub requests_total: AtomicU64,
    pub errors_total: AtomicU64,
    pub tokens_generated_total: AtomicU64,
    pub total_latency_ms: AtomicU64,
    pub latency_samples: Mutex<VecDeque<u64>>,

    // Memory metrics (formerly LearnedMemory)
    pub peak_vram_mb: AtomicU64,
    pub peak_sysmem_mb: AtomicU64,
    pub sample_count: AtomicU64,
    pub last_memory_update: Mutex<String>,

    // Human-readable names that map to this hash (e.g., "gpt-oss-20b:fast")
    pub display_names: Mutex<HashSet<String>>,

    /// Model parameters parsed from the most recent instance startup for this
    /// hash. Persisted so `/v1/models` can report them when no instance is
    /// running. The args hash pins them to the exact launch configuration:
    /// any cookbook change that affects the params also changes the hash. The
    /// llama.cpp version recorded alongside guards against serving values
    /// observed under a different binary (same args, upgraded llama.cpp).
    pub parsed_params: Mutex<Option<PersistedParsedParams>>,
}

/// Parsed model params plus the llama.cpp version they were observed under.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PersistedParsedParams {
    pub params: crate::instance::ParsedModelParams,
    /// llama.cpp version (commit) of the binary the instance was running, or
    /// `None` when no version had been recorded (e.g. managed builds
    /// disabled, or the instance started before the startup build check
    /// recorded one).
    #[serde(default)]
    pub llama_cpp_version: Option<String>,
}

impl Default for HashMetrics {
    fn default() -> Self {
        Self {
            requests_total: AtomicU64::new(0),
            errors_total: AtomicU64::new(0),
            tokens_generated_total: AtomicU64::new(0),
            total_latency_ms: AtomicU64::new(0),
            latency_samples: Mutex::new(VecDeque::with_capacity(100)),
            peak_vram_mb: AtomicU64::new(0),
            peak_sysmem_mb: AtomicU64::new(0),
            sample_count: AtomicU64::new(0),
            last_memory_update: Mutex::new(String::new()),
            display_names: Mutex::new(HashSet::new()),
            parsed_params: Mutex::new(None),
        }
    }
}

impl HashMetrics {
    pub fn observe_latency(&self, ms: u64) {
        self.total_latency_ms.fetch_add(ms, Ordering::Relaxed);
        let mut samples = self.latency_samples.lock();
        if samples.len() >= 100 {
            samples.pop_front();
        }
        samples.push_back(ms);
    }

    pub fn observe_memory(&self, vram_mb: u64, sysmem_mb: u64) {
        self.peak_vram_mb.fetch_max(vram_mb, Ordering::Relaxed);
        self.peak_sysmem_mb.fetch_max(sysmem_mb, Ordering::Relaxed);
        self.sample_count.fetch_add(1, Ordering::Relaxed);
        *self.last_memory_update.lock() = chrono::Utc::now().to_rfc3339();
    }

    pub fn add_display_name(&self, name: &str) {
        self.display_names.lock().insert(name.to_string());
    }

    /// Record the model parameters parsed from an instance startup log,
    /// together with the llama.cpp version that produced them. The latest
    /// parse wins: params come from the running binary, so a llama.cpp
    /// upgrade can legitimately change them for the same hash.
    pub fn set_parsed_params(
        &self,
        params: crate::instance::ParsedModelParams,
        llama_cpp_version: Option<String>,
    ) {
        *self.parsed_params.lock() = Some(PersistedParsedParams {
            params,
            llama_cpp_version,
        });
    }

    /// Get the persisted model parameters for this hash, if any instance for
    /// it has ever become ready.
    ///
    /// Params recorded under a *different* llama.cpp version than the one
    /// currently live are withheld: the same launch args can resolve to
    /// different effective values (slot count, context) on another build.
    /// When either version is unknown there is nothing to compare against,
    /// so the params are served as-is.
    pub fn get_parsed_params(
        &self,
        current_llama_cpp_version: Option<&str>,
    ) -> Option<crate::instance::ParsedModelParams> {
        let stored = self.parsed_params.lock().clone()?;
        match (
            stored.llama_cpp_version.as_deref(),
            current_llama_cpp_version,
        ) {
            (Some(stored_version), Some(current_version)) if stored_version != current_version => {
                None
            }
            _ => Some(stored.params),
        }
    }

    /// Get tokens per second based on accumulated metrics.
    pub fn tokens_per_second(&self) -> f64 {
        let tokens = self.tokens_generated_total.load(Ordering::Relaxed);
        let latency = self.total_latency_ms.load(Ordering::Relaxed);
        if latency > 0 {
            (tokens as f64 * 1000.0) / latency as f64
        } else {
            0.0
        }
    }
}

/// Why a request was dropped from a model queue. Mirrors the `reason` field
/// of `queue_drop` log events and the `reason` label of
/// `proxy_queue_drops_total`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum QueueDropReason {
    /// The per-model queue reached its `max_queue_size_per_model`.
    Full,
    /// The request's queue wait exceeded its timeout.
    Timeout,
    /// The node-wide `max_total_queue_entries` limit was reached.
    GlobalLimit,
}

#[derive(Debug, Default)]
pub struct Metrics {
    pub requests_total: AtomicU64,
    pub errors_total: AtomicU64,
    pub current_requests: AtomicU64,

    // Queue fairness metrics
    pub queue_pending_tokens: AtomicU64,
    /// Total queue wait time in milliseconds (for computing average)
    pub queue_wait_total_ms: AtomicU64,
    /// Number of requests that waited in queue
    pub queue_wait_count: AtomicU64,
    /// Current total queued requests across all model queues, matching
    /// `total_queue_length` in `/cluster/nodes`. Runtime gauge, refreshed by
    /// the scrape handlers; starts fresh on restart.
    pub queue_length: AtomicU64,
    /// Requests dropped from model queues because the per-model queue was
    /// full. Cumulative; persisted across restarts.
    pub queue_drops_full: AtomicU64,
    /// Requests dropped from model queues because their queue wait timed
    /// out. Cumulative; persisted across restarts.
    pub queue_drops_timeout: AtomicU64,
    /// Requests dropped because the node-wide queue entry limit
    /// (`max_total_queue_entries`) was reached. Cumulative; persisted across
    /// restarts.
    pub queue_drops_global_limit: AtomicU64,

    // Per-hash metrics: args_hash -> HashMetrics
    pub hash_metrics: RwLock<HashMap<String, Arc<HashMetrics>>>,

    // Current node resource accounting. These runtime gauges start fresh on restart.
    pub node_llamesh_vram_mb: AtomicU64,
    pub node_llamesh_sysmem_mb: AtomicU64,
    pub node_external_vram_mb: AtomicU64,
    pub node_effective_vram_mb: AtomicU64,
    pub node_effective_sysmem_mb: AtomicU64,
    pub node_device_vram_used_mb: AtomicU64,
    pub node_device_vram_total_mb: AtomicU64,
    pub node_gpu_telemetry_available: AtomicBool,
    /// Number of llama-server instances currently managed on this node
    /// (spawned and not yet evicted), matching `active_instances` in
    /// `/cluster/nodes`.
    pub node_active_instances: AtomicU64,
    /// Configured VRAM admission guardrail in MB (`max_vram_mb` from node
    /// config), mirrored here so the limit is scrapeable alongside the
    /// effective-usage gauges. Admission spawns a new instance only while
    /// `effective_vram + required <= max_vram_mb`, so this is the denominator
    /// for guardrail utilization (`effective / max`) and headroom
    /// (`max - effective`). It is a static config value, refreshed on every
    /// resource snapshot.
    pub node_max_vram_mb: AtomicU64,
    /// Configured system-memory admission guardrail in MB (`max_sysmem_mb` from
    /// node config). The system-memory counterpart to `node_max_vram_mb`.
    pub node_max_sysmem_mb: AtomicU64,
}

#[derive(Serialize, Deserialize)]
pub struct MetricsSnapshot {
    pub node_id: String,
    /// Version of the llamesh proxy itself (from `CARGO_PKG_VERSION`).
    /// `#[serde(default)]` keeps snapshots written by older versions loadable.
    #[serde(default)]
    pub version: String,
    pub llama_cpp_version: String,
    pub requests_total: u64,
    pub errors_total: u64,
    pub current_requests: u64,
    /// Current total queued requests across all model queues. Runtime gauge;
    /// not restored on load. `#[serde(default)]` keeps snapshots written by
    /// older versions loadable.
    #[serde(default)]
    pub queue_length: u64,
    /// Requests dropped from model queues by drop reason. Cumulative
    /// counters, restored on load like `requests_total`/`errors_total`.
    /// `#[serde(default)]` keeps snapshots written by older versions loadable
    /// (a missing field must not reset every other counter).
    #[serde(default)]
    pub queue_drops_full: u64,
    #[serde(default)]
    pub queue_drops_timeout: u64,
    #[serde(default)]
    pub queue_drops_global_limit: u64,
    /// In-flight queue tokens (pending slot reservations awaiting a notified
    /// waiter). Runtime gauge mirrored here so `/metrics/json` matches the
    /// Prometheus `proxy_queue_pending_tokens`; like `queue_length` it starts
    /// fresh on restart and is not restored on load. `#[serde(default)]` keeps
    /// snapshots written by older versions loadable.
    #[serde(default)]
    pub queue_pending_tokens: u64,
    /// Cumulative queue wait time (ms) and the number of requests that waited;
    /// together they give the mean queue wait (`queue_wait_total_ms /
    /// queue_wait_count`). Mirrored so `/metrics/json` matches the Prometheus
    /// `proxy_queue_wait_total_ms` / `proxy_queue_wait_count`. Like those
    /// counters they start fresh on restart, so they are serialized for
    /// observability but not restored on load. `#[serde(default)]` keeps older
    /// snapshots loadable.
    #[serde(default)]
    pub queue_wait_total_ms: u64,
    #[serde(default)]
    pub queue_wait_count: u64,
    /// Times token counting was disabled for a response (token usage
    /// unavailable) and stream cleanups skipped during shutdown. Process-global
    /// counters mirrored so `/metrics/json` matches the Prometheus
    /// `proxy_token_counting_disabled_total` /
    /// `proxy_skipped_stream_cleanups_total`; they reset on restart like their
    /// Prometheus counterparts. `#[serde(default)]` keeps older snapshots
    /// loadable.
    #[serde(default)]
    pub token_counting_disabled_total: u64,
    #[serde(default)]
    pub skipped_stream_cleanups_total: u64,
    pub hashes: HashMap<String, HashMetricsSnapshot>,
    pub updated_at: String,
    #[serde(default)]
    pub is_building: bool,
    #[serde(default)]
    pub last_build_error: Option<String>,
    #[serde(default)]
    pub last_build_at: Option<String>,
    #[serde(default)]
    pub node_resources: NodeResourceMetricsSnapshot,
}

#[derive(Debug, Default, Serialize, Deserialize, Clone)]
pub struct NodeResourceMetricsSnapshot {
    pub llamesh_vram_mb: u64,
    pub llamesh_sysmem_mb: u64,
    pub external_vram_mb: u64,
    pub effective_vram_mb: u64,
    pub effective_sysmem_mb: u64,
    pub device_vram_used_mb: u64,
    pub device_vram_total_mb: u64,
    pub gpu_telemetry_available: bool,
    /// Number of llama-server instances managed on this node. `#[serde(default)]`
    /// keeps snapshots written by older versions (without this field) loadable.
    #[serde(default)]
    pub active_instances: u64,
    /// Configured VRAM admission guardrail in MB (`max_vram_mb` from node
    /// config). `#[serde(default)]` keeps snapshots written by older versions
    /// (without this field) loadable.
    #[serde(default)]
    pub max_vram_mb: u64,
    /// Configured system-memory admission guardrail in MB (`max_sysmem_mb` from
    /// node config). `#[serde(default)]` keeps older snapshots loadable.
    #[serde(default)]
    pub max_sysmem_mb: u64,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct HashMetricsSnapshot {
    pub display_names: Vec<String>,
    pub requests_total: u64,
    pub errors_total: u64,
    pub tokens_generated_total: u64,
    pub avg_latency_ms: f64,
    #[serde(default)]
    pub p95_latency_ms: u64,
    pub peak_vram_mb: u64,
    pub peak_sysmem_mb: u64,
    pub sample_count: u64,
    /// Parsed model params from the most recent instance startup, with the
    /// llama.cpp version they were observed under.
    /// `#[serde(default)]` keeps snapshots written by older versions loadable,
    /// and the lenient deserializer drops (rather than fails on) a value whose
    /// shape doesn't match — a parse error here would otherwise discard the
    /// entire snapshot, silently resetting every persisted counter.
    #[serde(default, deserialize_with = "lenient_parsed_params")]
    pub parsed_model_params: Option<PersistedParsedParams>,
}

/// Deserialize `parsed_model_params`, tolerating unknown shapes.
///
/// Learned data is valuable but reconstructible; the surrounding counters are
/// not. If this field ever drifts in shape (as opposed to being absent, which
/// `default` already handles), losing just the params is strictly better than
/// `Metrics::load` discarding the whole snapshot.
fn lenient_parsed_params<'de, D>(deserializer: D) -> Result<Option<PersistedParsedParams>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    Ok(serde_json::from_value(value).unwrap_or(None))
}

impl Metrics {
    pub fn new() -> Self {
        Self::default()
    }

    pub async fn load(path: &std::path::Path) -> Self {
        if !path.exists() {
            return Self::new();
        }

        match tokio::fs::read(path).await {
            Ok(bytes) => match serde_json::from_slice::<MetricsSnapshot>(&bytes) {
                Ok(snapshot) => Self::from_snapshot(snapshot),
                Err(e) => {
                    tracing::warn!("Failed to parse metrics snapshot: {}", e);
                    Self::new()
                }
            },
            Err(e) => {
                tracing::warn!("Failed to read metrics snapshot: {}", e);
                Self::new()
            }
        }
    }

    fn from_snapshot(snapshot: MetricsSnapshot) -> Self {
        let m = Self::new();
        m.requests_total
            .store(snapshot.requests_total, Ordering::Relaxed);
        m.errors_total
            .store(snapshot.errors_total, Ordering::Relaxed);
        // current_requests starts at 0 on restart
        m.current_requests.store(0, Ordering::Relaxed);

        let mut map = HashMap::new();
        for (hash, v) in snapshot.hashes {
            let hm = HashMetrics {
                requests_total: AtomicU64::new(v.requests_total),
                errors_total: AtomicU64::new(v.errors_total),
                tokens_generated_total: AtomicU64::new(v.tokens_generated_total),
                total_latency_ms: AtomicU64::new(
                    (v.avg_latency_ms * v.requests_total as f64) as u64,
                ),
                latency_samples: Mutex::new(VecDeque::new()), // Samples lost on restart, acceptable
                peak_vram_mb: AtomicU64::new(v.peak_vram_mb),
                peak_sysmem_mb: AtomicU64::new(v.peak_sysmem_mb),
                sample_count: AtomicU64::new(v.sample_count),
                last_memory_update: Mutex::new(String::new()),
                display_names: Mutex::new(v.display_names.into_iter().collect()),
                parsed_params: Mutex::new(v.parsed_model_params),
            };
            map.insert(hash, Arc::new(hm));
        }

        Metrics {
            requests_total: m.requests_total,
            errors_total: m.errors_total,
            current_requests: m.current_requests,
            queue_pending_tokens: AtomicU64::new(0), // Not persisted, starts fresh
            queue_wait_total_ms: AtomicU64::new(0),  // Not persisted, starts fresh
            queue_wait_count: AtomicU64::new(0),     // Not persisted, starts fresh
            queue_length: AtomicU64::new(0),         // Runtime gauge, starts fresh
            queue_drops_full: AtomicU64::new(snapshot.queue_drops_full),
            queue_drops_timeout: AtomicU64::new(snapshot.queue_drops_timeout),
            queue_drops_global_limit: AtomicU64::new(snapshot.queue_drops_global_limit),
            hash_metrics: RwLock::new(map),
            node_llamesh_vram_mb: AtomicU64::new(0),
            node_llamesh_sysmem_mb: AtomicU64::new(0),
            node_external_vram_mb: AtomicU64::new(0),
            node_effective_vram_mb: AtomicU64::new(0),
            node_effective_sysmem_mb: AtomicU64::new(0),
            node_device_vram_used_mb: AtomicU64::new(0),
            node_device_vram_total_mb: AtomicU64::new(0),
            node_gpu_telemetry_available: AtomicBool::new(false),
            node_active_instances: AtomicU64::new(0),
            node_max_vram_mb: AtomicU64::new(0),
            node_max_sysmem_mb: AtomicU64::new(0),
        }
    }

    pub fn inc_requests(&self) {
        self.requests_total.fetch_add(1, Ordering::Relaxed);
        self.current_requests.fetch_add(1, Ordering::Relaxed);
    }

    pub fn dec_current_requests(&self) {
        // Use saturating subtraction to prevent underflow wrapping to u64::MAX
        // This can happen if drop handler runs twice or in race conditions
        let prev = self.current_requests.fetch_sub(1, Ordering::Relaxed);
        if prev == 0 {
            // Underflow occurred - correct it back to 0
            self.current_requests.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                event = "metrics_underflow",
                metric = "current_requests",
                "Request counter underflow detected and corrected"
            );
        }
    }

    pub fn inc_errors(&self) {
        self.errors_total.fetch_add(1, Ordering::Relaxed);
    }

    /// Increment pending tokens count (called when a waiter is notified)
    pub fn inc_pending_tokens(&self) {
        self.queue_pending_tokens.fetch_add(1, Ordering::Relaxed);
    }

    /// Decrement pending tokens count (called when waiter acquires slot or times out)
    pub fn dec_pending_tokens(&self) {
        // Use saturating subtraction to prevent underflow wrapping to u64::MAX
        let prev = self.queue_pending_tokens.fetch_sub(1, Ordering::Relaxed);
        if prev == 0 {
            // Underflow occurred - correct it back to 0
            self.queue_pending_tokens.fetch_add(1, Ordering::Relaxed);
            tracing::warn!(
                event = "metrics_underflow",
                metric = "queue_pending_tokens",
                "Pending tokens counter underflow detected and corrected"
            );
        }
    }

    /// Record queue wait time in milliseconds
    pub fn observe_queue_wait(&self, ms: u64) {
        self.queue_wait_total_ms.fetch_add(ms, Ordering::Relaxed);
        self.queue_wait_count.fetch_add(1, Ordering::Relaxed);
    }

    /// Refresh the current total queue depth gauge.
    pub fn set_queue_length(&self, length: u64) {
        self.queue_length.store(length, Ordering::Relaxed);
    }

    /// Record a request dropped from a model queue.
    pub fn record_queue_drop(&self, reason: QueueDropReason) {
        let counter = match reason {
            QueueDropReason::Full => &self.queue_drops_full,
            QueueDropReason::Timeout => &self.queue_drops_timeout,
            QueueDropReason::GlobalLimit => &self.queue_drops_global_limit,
        };
        counter.fetch_add(1, Ordering::Relaxed);
    }

    /// Get or create metrics for a given args_hash.
    pub async fn get_hash_metrics(&self, args_hash: &str) -> Arc<HashMetrics> {
        {
            let map = self.hash_metrics.read().await;
            if let Some(m) = map.get(args_hash) {
                return m.clone();
            }
        }

        let mut map = self.hash_metrics.write().await;
        map.entry(args_hash.to_string())
            .or_insert_with(|| Arc::new(HashMetrics::default()))
            .clone()
    }

    /// Get learned memory for a given args_hash (for backward compat during transition).
    pub async fn get_learned_memory(&self, args_hash: &str) -> Option<(u64, u64)> {
        let map = self.hash_metrics.read().await;
        map.get(args_hash).and_then(|m| {
            if m.sample_count.load(Ordering::Relaxed) == 0 {
                return None;
            }
            Some((
                m.peak_vram_mb.load(Ordering::Relaxed),
                m.peak_sysmem_mb.load(Ordering::Relaxed),
            ))
        })
    }

    pub fn observe_node_resources(&self, snapshot: NodeResourceMetricsSnapshot) {
        self.node_llamesh_vram_mb
            .store(snapshot.llamesh_vram_mb, Ordering::Relaxed);
        self.node_llamesh_sysmem_mb
            .store(snapshot.llamesh_sysmem_mb, Ordering::Relaxed);
        self.node_external_vram_mb
            .store(snapshot.external_vram_mb, Ordering::Relaxed);
        self.node_effective_vram_mb
            .store(snapshot.effective_vram_mb, Ordering::Relaxed);
        self.node_effective_sysmem_mb
            .store(snapshot.effective_sysmem_mb, Ordering::Relaxed);
        self.node_device_vram_used_mb
            .store(snapshot.device_vram_used_mb, Ordering::Relaxed);
        self.node_device_vram_total_mb
            .store(snapshot.device_vram_total_mb, Ordering::Relaxed);
        self.node_gpu_telemetry_available
            .store(snapshot.gpu_telemetry_available, Ordering::Relaxed);
        self.node_active_instances
            .store(snapshot.active_instances, Ordering::Relaxed);
        self.node_max_vram_mb
            .store(snapshot.max_vram_mb, Ordering::Relaxed);
        self.node_max_sysmem_mb
            .store(snapshot.max_sysmem_mb, Ordering::Relaxed);
    }

    /// Check if we have any memory data for a given args_hash (for cold start detection).
    pub async fn has_memory_data(&self, args_hash: &str) -> bool {
        let map = self.hash_metrics.read().await;
        map.get(args_hash)
            .map(|m| m.sample_count.load(Ordering::Relaxed) > 0)
            .unwrap_or(false)
    }

    pub async fn snapshot(
        &self,
        node_id: String,
        version: String,
        build_status: Option<crate::build_manager::BuildStatus>,
    ) -> MetricsSnapshot {
        let mut hashes = HashMap::new();
        let map = self.hash_metrics.read().await;

        for (hash, v) in map.iter() {
            let reqs = v.requests_total.load(Ordering::Relaxed);
            let latency = v.total_latency_ms.load(Ordering::Relaxed);
            let avg = if reqs > 0 {
                latency as f64 / reqs as f64
            } else {
                0.0
            };

            let p95 = {
                let mut samples: Vec<u64> = v.latency_samples.lock().iter().cloned().collect();
                if samples.is_empty() {
                    0
                } else {
                    samples.sort_unstable();
                    // Proper percentile: for n samples, 95th percentile is at index floor((n-1) * 0.95)
                    let idx = ((samples.len() - 1) as f64 * 0.95).floor() as usize;
                    samples[idx]
                }
            };

            let display_names: Vec<String> = v.display_names.lock().iter().cloned().collect();

            hashes.insert(
                hash.clone(),
                HashMetricsSnapshot {
                    display_names,
                    requests_total: reqs,
                    errors_total: v.errors_total.load(Ordering::Relaxed),
                    tokens_generated_total: v.tokens_generated_total.load(Ordering::Relaxed),
                    avg_latency_ms: avg,
                    p95_latency_ms: p95,
                    peak_vram_mb: v.peak_vram_mb.load(Ordering::Relaxed),
                    peak_sysmem_mb: v.peak_sysmem_mb.load(Ordering::Relaxed),
                    sample_count: v.sample_count.load(Ordering::Relaxed),
                    parsed_model_params: v.parsed_params.lock().clone(),
                },
            );
        }

        MetricsSnapshot {
            node_id,
            version: env!("CARGO_PKG_VERSION").to_string(),
            llama_cpp_version: version,
            requests_total: self.requests_total.load(Ordering::Relaxed),
            errors_total: self.errors_total.load(Ordering::Relaxed),
            current_requests: self.current_requests.load(Ordering::Relaxed),
            queue_length: self.queue_length.load(Ordering::Relaxed),
            queue_drops_full: self.queue_drops_full.load(Ordering::Relaxed),
            queue_drops_timeout: self.queue_drops_timeout.load(Ordering::Relaxed),
            queue_drops_global_limit: self.queue_drops_global_limit.load(Ordering::Relaxed),
            queue_pending_tokens: self.queue_pending_tokens.load(Ordering::Relaxed),
            queue_wait_total_ms: self.queue_wait_total_ms.load(Ordering::Relaxed),
            queue_wait_count: self.queue_wait_count.load(Ordering::Relaxed),
            token_counting_disabled_total: crate::router::TOKEN_COUNTING_DISABLED
                .load(Ordering::Relaxed),
            skipped_stream_cleanups_total: crate::router::SKIPPED_STREAM_CLEANUPS
                .load(Ordering::Relaxed),
            hashes,
            updated_at: chrono::Utc::now().to_rfc3339(),
            is_building: build_status
                .as_ref()
                .map(|s| s.is_building)
                .unwrap_or(false),
            last_build_error: build_status
                .as_ref()
                .and_then(|s| s.last_build_error.clone()),
            last_build_at: build_status.and_then(|s| s.last_build_at),
            node_resources: NodeResourceMetricsSnapshot {
                llamesh_vram_mb: self.node_llamesh_vram_mb.load(Ordering::Relaxed),
                llamesh_sysmem_mb: self.node_llamesh_sysmem_mb.load(Ordering::Relaxed),
                external_vram_mb: self.node_external_vram_mb.load(Ordering::Relaxed),
                effective_vram_mb: self.node_effective_vram_mb.load(Ordering::Relaxed),
                effective_sysmem_mb: self.node_effective_sysmem_mb.load(Ordering::Relaxed),
                device_vram_used_mb: self.node_device_vram_used_mb.load(Ordering::Relaxed),
                device_vram_total_mb: self.node_device_vram_total_mb.load(Ordering::Relaxed),
                gpu_telemetry_available: self.node_gpu_telemetry_available.load(Ordering::Relaxed),
                active_instances: self.node_active_instances.load(Ordering::Relaxed),
                max_vram_mb: self.node_max_vram_mb.load(Ordering::Relaxed),
                max_sysmem_mb: self.node_max_sysmem_mb.load(Ordering::Relaxed),
            },
        }
    }
}

use crate::circuit_breaker::{CircuitBreaker, CircuitState};

pub async fn render_prometheus_with_circuit_breaker(
    metrics: &Metrics,
    circuit_breaker: Option<&CircuitBreaker>,
    llama_cpp_version: &str,
) -> String {
    let mut out = String::new();

    // Build info as a constant gauge carrying identifying labels. This is the
    // conventional Prometheus pattern for exposing version (cf. `*_build_info`):
    // the value is always 1 and rollout/skew is tracked via the labels, e.g.
    // `count by (version) (proxy_build_info)` or
    // `count by (llama_cpp_version) (proxy_build_info)`. The proxy `version`
    // comes from `CARGO_PKG_VERSION`, a compile-time constant that is always a
    // valid semver, so it needs no escaping; `llama_cpp_version` is derived from
    // git output at runtime, so it is passed through `escape_label_value` for
    // safety. Both labels mirror the `/metrics/json` snapshot, which carries the
    // same two version fields.
    out.push_str(
        "# HELP proxy_build_info Build information for the llamesh proxy; value is always 1.\n",
    );
    out.push_str("# TYPE proxy_build_info gauge\n");
    out.push_str(&format!(
        "proxy_build_info{{version=\"{}\",llama_cpp_version=\"{}\"}} 1\n",
        env!("CARGO_PKG_VERSION"),
        escape_label_value(llama_cpp_version)
    ));

    // Global metrics with TYPE/HELP headers for Prometheus spec compliance
    out.push_str("# HELP proxy_requests_total Total number of requests received.\n");
    out.push_str("# TYPE proxy_requests_total counter\n");
    out.push_str(&format!(
        "proxy_requests_total {}\n",
        metrics.requests_total.load(Ordering::Relaxed)
    ));
    out.push_str("# HELP proxy_errors_total Total number of request errors.\n");
    out.push_str("# TYPE proxy_errors_total counter\n");
    out.push_str(&format!(
        "proxy_errors_total {}\n",
        metrics.errors_total.load(Ordering::Relaxed)
    ));
    out.push_str("# HELP proxy_current_requests Number of requests currently in flight.\n");
    out.push_str("# TYPE proxy_current_requests gauge\n");
    out.push_str(&format!(
        "proxy_current_requests {}\n",
        metrics.current_requests.load(Ordering::Relaxed)
    ));
    out.push_str(
        "# HELP proxy_node_llamesh_vram_mb VRAM attributed to llamesh-managed instances.\n",
    );
    out.push_str("# TYPE proxy_node_llamesh_vram_mb gauge\n");
    out.push_str(&format!(
        "proxy_node_llamesh_vram_mb {}\n",
        metrics.node_llamesh_vram_mb.load(Ordering::Relaxed)
    ));
    out.push_str("# HELP proxy_node_llamesh_sysmem_mb System memory attributed to llamesh-managed instances.\n");
    out.push_str("# TYPE proxy_node_llamesh_sysmem_mb gauge\n");
    out.push_str(&format!(
        "proxy_node_llamesh_sysmem_mb {}\n",
        metrics.node_llamesh_sysmem_mb.load(Ordering::Relaxed)
    ));
    out.push_str(
        "# HELP proxy_node_external_vram_mb Device VRAM used outside llamesh-managed instances.\n",
    );
    out.push_str("# TYPE proxy_node_external_vram_mb gauge\n");
    out.push_str(&format!(
        "proxy_node_external_vram_mb {}\n",
        metrics.node_external_vram_mb.load(Ordering::Relaxed)
    ));
    out.push_str("# HELP proxy_node_effective_vram_mb VRAM used for admission control after external usage is included.\n");
    out.push_str("# TYPE proxy_node_effective_vram_mb gauge\n");
    out.push_str(&format!(
        "proxy_node_effective_vram_mb {}\n",
        metrics.node_effective_vram_mb.load(Ordering::Relaxed)
    ));
    out.push_str(
        "# HELP proxy_node_effective_sysmem_mb System memory used for admission control.\n",
    );
    out.push_str("# TYPE proxy_node_effective_sysmem_mb gauge\n");
    out.push_str(&format!(
        "proxy_node_effective_sysmem_mb {}\n",
        metrics.node_effective_sysmem_mb.load(Ordering::Relaxed)
    ));
    out.push_str(
        "# HELP proxy_node_device_vram_used_mb Device-wide used VRAM reported by GPU telemetry.\n",
    );
    out.push_str("# TYPE proxy_node_device_vram_used_mb gauge\n");
    out.push_str(&format!(
        "proxy_node_device_vram_used_mb {}\n",
        metrics.node_device_vram_used_mb.load(Ordering::Relaxed)
    ));
    out.push_str("# HELP proxy_node_device_vram_total_mb Device-wide total VRAM reported by GPU telemetry.\n");
    out.push_str("# TYPE proxy_node_device_vram_total_mb gauge\n");
    out.push_str(&format!(
        "proxy_node_device_vram_total_mb {}\n",
        metrics.node_device_vram_total_mb.load(Ordering::Relaxed)
    ));
    out.push_str("# HELP proxy_node_gpu_telemetry_available Whether device-wide GPU telemetry is available (1=true, 0=false).\n");
    out.push_str("# TYPE proxy_node_gpu_telemetry_available gauge\n");
    out.push_str(&format!(
        "proxy_node_gpu_telemetry_available {}\n",
        if metrics.node_gpu_telemetry_available.load(Ordering::Relaxed) {
            1
        } else {
            0
        }
    ));
    out.push_str(
        "# HELP proxy_node_active_instances Number of llama-server instances currently managed on this node.\n",
    );
    out.push_str("# TYPE proxy_node_active_instances gauge\n");
    out.push_str(&format!(
        "proxy_node_active_instances {}\n",
        metrics.node_active_instances.load(Ordering::Relaxed)
    ));
    out.push_str("# HELP proxy_node_max_vram_mb Configured VRAM admission guardrail in MB (max_vram_mb); a new instance is spawned only while effective VRAM plus its estimated usage stays within this limit.\n");
    out.push_str("# TYPE proxy_node_max_vram_mb gauge\n");
    out.push_str(&format!(
        "proxy_node_max_vram_mb {}\n",
        metrics.node_max_vram_mb.load(Ordering::Relaxed)
    ));
    out.push_str("# HELP proxy_node_max_sysmem_mb Configured system-memory admission guardrail in MB (max_sysmem_mb).\n");
    out.push_str("# TYPE proxy_node_max_sysmem_mb gauge\n");
    out.push_str(&format!(
        "proxy_node_max_sysmem_mb {}\n",
        metrics.node_max_sysmem_mb.load(Ordering::Relaxed)
    ));

    // Queue fairness metrics
    out.push_str("# HELP proxy_queue_pending_tokens Number of pending queue tokens.\n");
    out.push_str("# TYPE proxy_queue_pending_tokens gauge\n");
    out.push_str(&format!(
        "proxy_queue_pending_tokens {}\n",
        metrics.queue_pending_tokens.load(Ordering::Relaxed)
    ));
    out.push_str("# HELP proxy_queue_wait_total_ms Total queue wait time in milliseconds.\n");
    out.push_str("# TYPE proxy_queue_wait_total_ms counter\n");
    out.push_str(&format!(
        "proxy_queue_wait_total_ms {}\n",
        metrics.queue_wait_total_ms.load(Ordering::Relaxed)
    ));
    out.push_str("# HELP proxy_queue_wait_count Total number of requests that waited in queue.\n");
    out.push_str("# TYPE proxy_queue_wait_count counter\n");
    out.push_str(&format!(
        "proxy_queue_wait_count {}\n",
        metrics.queue_wait_count.load(Ordering::Relaxed)
    ));
    out.push_str("# HELP proxy_queue_length Current number of requests waiting in model queues.\n");
    out.push_str("# TYPE proxy_queue_length gauge\n");
    out.push_str(&format!(
        "proxy_queue_length {}\n",
        metrics.queue_length.load(Ordering::Relaxed)
    ));
    out.push_str("# HELP proxy_queue_drops_total Requests dropped from model queues, by reason.\n");
    out.push_str("# TYPE proxy_queue_drops_total counter\n");
    out.push_str(&format!(
        "proxy_queue_drops_total{{reason=\"full\"}} {}\n",
        metrics.queue_drops_full.load(Ordering::Relaxed)
    ));
    out.push_str(&format!(
        "proxy_queue_drops_total{{reason=\"timeout\"}} {}\n",
        metrics.queue_drops_timeout.load(Ordering::Relaxed)
    ));
    out.push_str(&format!(
        "proxy_queue_drops_total{{reason=\"global_limit\"}} {}\n",
        metrics.queue_drops_global_limit.load(Ordering::Relaxed)
    ));
    out.push_str("# HELP proxy_token_counting_disabled_total Times token counting was disabled.\n");
    out.push_str("# TYPE proxy_token_counting_disabled_total counter\n");
    out.push_str(&format!(
        "proxy_token_counting_disabled_total {}\n",
        crate::router::TOKEN_COUNTING_DISABLED.load(Ordering::Relaxed)
    ));

    // Skipped stream cleanups (cleanup failed during shutdown)
    out.push_str(
        "# HELP proxy_skipped_stream_cleanups_total Stream cleanups skipped during shutdown.\n",
    );
    out.push_str("# TYPE proxy_skipped_stream_cleanups_total counter\n");
    out.push_str(&format!(
        "proxy_skipped_stream_cleanups_total {}\n",
        crate::router::SKIPPED_STREAM_CLEANUPS.load(Ordering::Relaxed)
    ));

    // Circuit breaker metrics (if available)
    if let Some(cb) = circuit_breaker {
        out.push_str("# HELP proxy_circuit_breaker_state Circuit breaker state per peer (0=closed, 1=open, 2=half-open).\n");
        out.push_str("# TYPE proxy_circuit_breaker_state gauge\n");
        let states = cb.get_all_states().await;
        for (peer_id, state) in states {
            let state_value = match state {
                CircuitState::Closed => 0,
                CircuitState::Open => 1,
                CircuitState::HalfOpen => 2,
            };
            out.push_str(&format!(
                "proxy_circuit_breaker_state{{peer=\"{}\"}} {}\n",
                escape_label_value(&peer_id),
                state_value
            ));
        }
    }

    // Per-hash (model/profile) metrics.
    //
    // The Prometheus exposition format requires every sample of a metric
    // family to be contiguous, preceded by that family's HELP/TYPE. Emitting
    // all the HELP/TYPE lines once and then a per-hash block (requests, errors,
    // tokens, … for hash A; then the same for hash B) interleaves the families,
    // which strict parsers (OpenMetrics, `promtool check metrics`) split into
    // one-sample fragments. Snapshot each hash's values once, then emit one
    // fully grouped family at a time.
    struct HashRow {
        labels: String,
        requests: u64,
        errors: u64,
        tokens: u64,
        total_latency_ms: u64,
        p95_latency_ms: u64,
        peak_vram_mb: u64,
        peak_sysmem_mb: u64,
    }

    let rows: Vec<HashRow> = {
        let map = metrics.hash_metrics.read().await;
        map.iter()
            .map(|(hash, m)| {
                let display_names: Vec<String> = m.display_names.lock().iter().cloned().collect();
                let labels = format!(
                    "{{hash=\"{}\",names=\"{}\"}}",
                    escape_label_value(hash),
                    escape_label_value(&display_names.join(","))
                );
                let p95_latency_ms = {
                    let mut samples: Vec<u64> = m.latency_samples.lock().iter().cloned().collect();
                    if samples.is_empty() {
                        0
                    } else {
                        samples.sort_unstable();
                        let idx = ((samples.len() - 1) as f64 * 0.95).floor() as usize;
                        samples[idx.min(samples.len() - 1)]
                    }
                };
                HashRow {
                    labels,
                    requests: m.requests_total.load(Ordering::Relaxed),
                    errors: m.errors_total.load(Ordering::Relaxed),
                    tokens: m.tokens_generated_total.load(Ordering::Relaxed),
                    total_latency_ms: m.total_latency_ms.load(Ordering::Relaxed),
                    p95_latency_ms,
                    peak_vram_mb: m.peak_vram_mb.load(Ordering::Relaxed),
                    peak_sysmem_mb: m.peak_sysmem_mb.load(Ordering::Relaxed),
                }
            })
            .collect()
    };

    // Emit one fully grouped family: HELP, TYPE, then every hash's sample.
    let mut push_family =
        |name: &str, help: &str, metric_type: &str, value: &dyn Fn(&HashRow) -> u64| {
            out.push_str(&format!("# HELP {name} {help}\n"));
            out.push_str(&format!("# TYPE {name} {metric_type}\n"));
            for row in &rows {
                out.push_str(&format!("{}{} {}\n", name, row.labels, value(row)));
            }
        };

    push_family(
        "proxy_hash_requests_total",
        "Total requests per model/profile hash.",
        "counter",
        &|r| r.requests,
    );
    push_family(
        "proxy_hash_errors_total",
        "Total errors per model/profile hash.",
        "counter",
        &|r| r.errors,
    );
    push_family(
        "proxy_hash_tokens_generated_total",
        "Total tokens generated per hash.",
        "counter",
        &|r| r.tokens,
    );
    push_family(
        "proxy_hash_total_latency_ms",
        "Total latency in milliseconds per hash.",
        "counter",
        &|r| r.total_latency_ms,
    );
    push_family(
        "proxy_hash_p95_latency_ms",
        "P95 latency in milliseconds (last 100 samples).",
        "gauge",
        &|r| r.p95_latency_ms,
    );
    push_family(
        "proxy_hash_peak_vram_mb",
        "Peak VRAM usage in MB per hash.",
        "gauge",
        &|r| r.peak_vram_mb,
    );
    push_family(
        "proxy_hash_peak_sysmem_mb",
        "Peak system memory usage in MB per hash.",
        "gauge",
        &|r| r.peak_sysmem_mb,
    );

    out
}

fn escape_label_value(s: &str) -> String {
    s.replace('\\', "\\\\")
        .replace('"', "\\\"")
        .replace('\n', "\\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_escape_label_value() {
        assert_eq!(escape_label_value("normal"), "normal");
        assert_eq!(escape_label_value("with\"quote"), "with\\\"quote");
        assert_eq!(escape_label_value("with\\slash"), "with\\\\slash");
        assert_eq!(escape_label_value("multi\nline"), "multi\\nline");
    }

    #[tokio::test]
    async fn test_metrics_collection() {
        let metrics = Metrics::new();

        metrics.inc_requests();
        metrics.inc_requests();
        metrics.inc_errors();
        metrics.dec_current_requests(); // 2 -> 1

        assert_eq!(metrics.requests_total.load(Ordering::Relaxed), 2);
        assert_eq!(metrics.errors_total.load(Ordering::Relaxed), 1);
        assert_eq!(metrics.current_requests.load(Ordering::Relaxed), 1);
    }

    #[tokio::test]
    async fn test_hash_metrics() {
        let metrics = Metrics::new();
        let args_hash = "abc123def456";

        let hm = metrics.get_hash_metrics(args_hash).await;
        hm.add_display_name("gpt:fast");
        hm.requests_total.fetch_add(5, Ordering::Relaxed);
        hm.errors_total.fetch_add(1, Ordering::Relaxed);
        hm.tokens_generated_total.fetch_add(100, Ordering::Relaxed);

        // Add some latencies
        hm.observe_latency(100);
        hm.observe_latency(200);
        hm.observe_memory(1000, 2000);
        metrics.observe_node_resources(NodeResourceMetricsSnapshot {
            llamesh_vram_mb: 3000,
            llamesh_sysmem_mb: 4000,
            external_vram_mb: 500,
            effective_vram_mb: 3500,
            effective_sysmem_mb: 4000,
            device_vram_used_mb: 3500,
            device_vram_total_mb: 24000,
            gpu_telemetry_available: true,
            active_instances: 2,
            max_vram_mb: 23500,
            max_sysmem_mb: 185000,
        });

        let snapshot = metrics
            .snapshot("node-test".into(), "v1".into(), None)
            .await;
        let hash_snap = snapshot
            .hashes
            .get(args_hash)
            .expect("Hash metrics missing");

        assert_eq!(hash_snap.requests_total, 5);
        assert_eq!(hash_snap.errors_total, 1);
        assert_eq!(hash_snap.tokens_generated_total, 100);
        assert_eq!(hash_snap.avg_latency_ms, 60.0); // 300 / 5
        assert_eq!(hash_snap.p95_latency_ms, 100); // 95th percentile of [100, 200]: idx=floor((2-1)*0.95)=0
        assert_eq!(hash_snap.peak_vram_mb, 1000);
        assert_eq!(hash_snap.peak_sysmem_mb, 2000);
        assert_eq!(hash_snap.sample_count, 1);
        assert!(hash_snap.display_names.contains(&"gpt:fast".to_string()));
        assert_eq!(snapshot.node_resources.llamesh_vram_mb, 3000);
        assert_eq!(snapshot.node_resources.external_vram_mb, 500);
        assert!(snapshot.node_resources.gpu_telemetry_available);
        assert_eq!(snapshot.node_resources.active_instances, 2);
        assert_eq!(snapshot.node_resources.max_vram_mb, 23500);
        assert_eq!(snapshot.node_resources.max_sysmem_mb, 185000);
    }

    #[tokio::test]
    async fn test_prometheus_rendering() {
        let metrics = Metrics::new();
        metrics.inc_requests();
        metrics.observe_node_resources(NodeResourceMetricsSnapshot {
            llamesh_vram_mb: 100,
            llamesh_sysmem_mb: 200,
            external_vram_mb: 50,
            effective_vram_mb: 150,
            effective_sysmem_mb: 200,
            device_vram_used_mb: 150,
            device_vram_total_mb: 1000,
            gpu_telemetry_available: true,
            active_instances: 3,
            max_vram_mb: 900,
            max_sysmem_mb: 4096,
        });

        let hm = metrics.get_hash_metrics("abc123").await;
        hm.add_display_name("gpt\"cool\"");
        hm.requests_total.fetch_add(1, Ordering::Relaxed);
        hm.observe_latency(50);

        let output = render_prometheus_with_circuit_breaker(&metrics, None, "abc123def").await;

        assert!(output.contains("proxy_requests_total 1"));
        assert!(output.contains("proxy_node_external_vram_mb 50"));
        assert!(output.contains("proxy_node_gpu_telemetry_available 1"));
        assert!(output.contains("proxy_node_active_instances 3"));
        assert!(output.contains("proxy_node_max_vram_mb 900"));
        assert!(output.contains("proxy_node_max_sysmem_mb 4096"));
        assert!(output.contains("proxy_hash_requests_total{hash=\"abc123\""));
        assert!(output.contains("names=\"gpt\\\"cool\\\"\""));
    }

    #[tokio::test]
    async fn test_prometheus_build_info() {
        let metrics = Metrics::new();
        let output = render_prometheus_with_circuit_breaker(&metrics, None, "6ed481eea").await;

        // build_info is rendered as a constant gauge carrying the proxy version
        // (the crate version at compile time) and the llama.cpp binary version
        // in labels, mirroring the `/metrics/json` snapshot.
        assert!(output.contains("# TYPE proxy_build_info gauge"));
        assert!(output.contains(&format!(
            "proxy_build_info{{version=\"{}\",llama_cpp_version=\"6ed481eea\"}} 1",
            env!("CARGO_PKG_VERSION")
        )));
    }

    #[tokio::test]
    async fn test_prometheus_build_info_escapes_llama_cpp_version() {
        // `llama_cpp_version` is derived from git output at runtime, so the
        // renderer must escape it for Prometheus label-value safety even though
        // commit hashes never contain special characters in practice.
        let metrics = Metrics::new();
        let output = render_prometheus_with_circuit_breaker(&metrics, None, "weird\"\\\nver").await;

        assert!(output.contains(&format!(
            "proxy_build_info{{version=\"{}\",llama_cpp_version=\"weird\\\"\\\\\\nver\"}} 1",
            env!("CARGO_PKG_VERSION")
        )));
    }

    #[test]
    fn snapshot_without_active_instances_field_still_loads() {
        // A node_resources object written by an older version lacks the fields
        // added since (`active_instances`, and the `max_vram_mb`/`max_sysmem_mb`
        // guardrail limits). `#[serde(default)]` on each of those fields must
        // keep the snapshot loadable so an in-place upgrade preserves persisted
        // counters (e.g. requests_total) instead of resetting them via the
        // parse-error fallback in `Metrics::load`. This JSON mirrors the
        // node_resources shape written by older releases.
        let json = r#"{
            "node_id": "n",
            "llama_cpp_version": "abc",
            "requests_total": 1421955,
            "errors_total": 10,
            "current_requests": 0,
            "hashes": {},
            "updated_at": "2026-05-30T00:00:00Z",
            "node_resources": {
                "llamesh_vram_mb": 0,
                "llamesh_sysmem_mb": 0,
                "external_vram_mb": 3686,
                "effective_vram_mb": 3686,
                "effective_sysmem_mb": 0,
                "device_vram_used_mb": 3686,
                "device_vram_total_mb": 24564,
                "gpu_telemetry_available": true
            }
        }"#;
        let snapshot: MetricsSnapshot =
            serde_json::from_str(json).expect("pre-1.9.0 snapshot must still deserialize");
        assert_eq!(snapshot.requests_total, 1_421_955);
        assert_eq!(snapshot.node_resources.active_instances, 0);
        assert_eq!(snapshot.node_resources.max_vram_mb, 0);
        assert_eq!(snapshot.node_resources.max_sysmem_mb, 0);
        // Queue metrics added in 1.11.0 default when absent.
        assert_eq!(snapshot.queue_length, 0);
        assert_eq!(snapshot.queue_drops_full, 0);
        assert_eq!(snapshot.queue_drops_timeout, 0);
        assert_eq!(snapshot.queue_drops_global_limit, 0);
        // Parity metrics added in 1.14.0 also default when absent, so an
        // in-place upgrade from an older snapshot stays loadable instead of
        // resetting every counter via the parse-error fallback.
        assert_eq!(snapshot.queue_pending_tokens, 0);
        assert_eq!(snapshot.queue_wait_total_ms, 0);
        assert_eq!(snapshot.queue_wait_count, 0);
        assert_eq!(snapshot.token_counting_disabled_total, 0);
        assert_eq!(snapshot.skipped_stream_cleanups_total, 0);
    }

    #[tokio::test]
    async fn queue_drop_counters_round_trip_through_snapshot() {
        let metrics = Metrics::new();
        metrics.record_queue_drop(QueueDropReason::Full);
        metrics.record_queue_drop(QueueDropReason::Timeout);
        metrics.record_queue_drop(QueueDropReason::Timeout);
        metrics.record_queue_drop(QueueDropReason::GlobalLimit);
        metrics.set_queue_length(7);

        let snapshot = metrics.snapshot("n".into(), "v".into(), None).await;
        assert_eq!(snapshot.queue_drops_full, 1);
        assert_eq!(snapshot.queue_drops_timeout, 2);
        assert_eq!(snapshot.queue_drops_global_limit, 1);
        assert_eq!(snapshot.queue_length, 7);

        // Drop counters are cumulative and survive a restart; the queue depth
        // is a runtime gauge and starts fresh.
        let restored = Metrics::from_snapshot(snapshot);
        assert_eq!(restored.queue_drops_full.load(Ordering::Relaxed), 1);
        assert_eq!(restored.queue_drops_timeout.load(Ordering::Relaxed), 2);
        assert_eq!(restored.queue_drops_global_limit.load(Ordering::Relaxed), 1);
        assert_eq!(restored.queue_length.load(Ordering::Relaxed), 0);
    }

    #[tokio::test]
    async fn queue_wait_and_pending_metrics_surface_in_snapshot() {
        // queue_pending_tokens, queue_wait_total_ms and queue_wait_count are
        // exposed by Prometheus; the JSON snapshot must surface the same values
        // so `/metrics/json` reaches parity with `/metrics`.
        let metrics = Metrics::new();
        metrics.inc_pending_tokens();
        metrics.inc_pending_tokens();
        metrics.observe_queue_wait(120);
        metrics.observe_queue_wait(80);

        let snapshot = metrics.snapshot("n".into(), "v".into(), None).await;
        assert_eq!(snapshot.queue_pending_tokens, 2);
        assert_eq!(snapshot.queue_wait_total_ms, 200);
        assert_eq!(snapshot.queue_wait_count, 2);

        // These are runtime metrics mirrored for observability, not durable
        // counters: like queue_length they start fresh after a restart.
        let restored = Metrics::from_snapshot(snapshot);
        assert_eq!(restored.queue_pending_tokens.load(Ordering::Relaxed), 0);
        assert_eq!(restored.queue_wait_total_ms.load(Ordering::Relaxed), 0);
        assert_eq!(restored.queue_wait_count.load(Ordering::Relaxed), 0);
    }

    #[test]
    fn parity_metrics_deserialize_from_snapshot_json() {
        // A snapshot carrying the parity metrics (added 1.14.0) deserializes
        // them, including the two process-global counters
        // (token_counting_disabled_total, skipped_stream_cleanups_total) that
        // the JSON mirrors from Prometheus rather than from the Metrics struct.
        let json = r#"{
            "node_id": "n",
            "llama_cpp_version": "abc",
            "requests_total": 5,
            "errors_total": 1,
            "current_requests": 0,
            "queue_pending_tokens": 3,
            "queue_wait_total_ms": 450,
            "queue_wait_count": 9,
            "token_counting_disabled_total": 4,
            "skipped_stream_cleanups_total": 2,
            "hashes": {},
            "updated_at": "2026-06-13T00:00:00Z"
        }"#;
        let snapshot: MetricsSnapshot =
            serde_json::from_str(json).expect("snapshot with parity metrics must deserialize");
        assert_eq!(snapshot.queue_pending_tokens, 3);
        assert_eq!(snapshot.queue_wait_total_ms, 450);
        assert_eq!(snapshot.queue_wait_count, 9);
        assert_eq!(snapshot.token_counting_disabled_total, 4);
        assert_eq!(snapshot.skipped_stream_cleanups_total, 2);
        // Unrelated persisted counters still load alongside the new fields.
        assert_eq!(snapshot.requests_total, 5);
        assert_eq!(snapshot.errors_total, 1);
    }

    #[tokio::test]
    async fn render_groups_per_hash_families_contiguously() {
        // The Prometheus exposition format requires all samples of a metric
        // family to be contiguous. With more than one hash present, an
        // interleaved layout would split each family apart; assert every
        // family's sample lines form a single contiguous run.
        let metrics = Metrics::new();
        for (hash, name) in [("hash-a", "model-a:default"), ("hash-b", "model-b:fast")] {
            let hm = metrics.get_hash_metrics(hash).await;
            hm.add_display_name(name);
            hm.requests_total.fetch_add(5, Ordering::Relaxed);
            hm.observe_latency(12);
            hm.observe_memory(1000, 2000);
        }

        let out = render_prometheus_with_circuit_breaker(&metrics, None, "test").await;

        // Two hashes are present, so each per-hash family must have two samples.
        for family in [
            "proxy_hash_requests_total",
            "proxy_hash_errors_total",
            "proxy_hash_tokens_generated_total",
            "proxy_hash_total_latency_ms",
            "proxy_hash_p95_latency_ms",
            "proxy_hash_peak_vram_mb",
            "proxy_hash_peak_sysmem_mb",
        ] {
            let sample_lines: Vec<usize> = out
                .lines()
                .enumerate()
                .filter(|(_, l)| l.starts_with(&format!("{family}{{")))
                .map(|(i, _)| i)
                .collect();
            assert_eq!(
                sample_lines.len(),
                2,
                "expected 2 samples for {family}, got {}",
                sample_lines.len()
            );
            // Contiguous: the two sample lines must be adjacent.
            assert_eq!(
                sample_lines[1] - sample_lines[0],
                1,
                "samples for {family} are not contiguous (interleaved family)"
            );
            // HELP/TYPE precede the family's samples exactly once.
            assert_eq!(
                out.matches(&format!("# TYPE {family} ")).count(),
                1,
                "expected exactly one TYPE line for {family}"
            );
        }
    }

    #[tokio::test]
    async fn render_includes_queue_length_and_drops() {
        let metrics = Metrics::new();
        metrics.set_queue_length(3);
        metrics.record_queue_drop(QueueDropReason::Timeout);

        let out = render_prometheus_with_circuit_breaker(&metrics, None, "test").await;
        assert!(out.contains("proxy_queue_length 3\n"));
        assert!(out.contains("proxy_queue_drops_total{reason=\"full\"} 0\n"));
        assert!(out.contains("proxy_queue_drops_total{reason=\"timeout\"} 1\n"));
        assert!(out.contains("proxy_queue_drops_total{reason=\"global_limit\"} 0\n"));
    }

    #[tokio::test]
    async fn test_snapshot_includes_proxy_version() {
        let metrics = Metrics::new();
        let snapshot = metrics
            .snapshot("node-a".to_string(), "llama-cpp-1234".to_string(), None)
            .await;

        // The proxy version is the crate version and is distinct from the
        // llama.cpp binary version carried in `llama_cpp_version`.
        assert_eq!(snapshot.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(snapshot.llama_cpp_version, "llama-cpp-1234");
    }

    #[test]
    fn test_snapshot_deserializes_without_version_field() {
        // Snapshots persisted by versions predating the `version` field must
        // still load (the field is `#[serde(default)]`).
        let legacy = r#"{
            "node_id": "node-a",
            "llama_cpp_version": "old",
            "requests_total": 7,
            "errors_total": 1,
            "current_requests": 0,
            "hashes": {},
            "updated_at": "2025-01-01T00:00:00Z"
        }"#;
        let snapshot: MetricsSnapshot =
            serde_json::from_str(legacy).expect("legacy snapshot should deserialize");
        assert_eq!(snapshot.version, "");
        assert_eq!(snapshot.requests_total, 7);
    }

    #[tokio::test]
    async fn test_parsed_params_survive_snapshot_round_trip() {
        let metrics = Metrics::new();
        let params = crate::instance::ParsedModelParams {
            n_ctx: Some(4096),
            n_ctx_train: Some(131072),
            n_slots: Some(4),
            arch: Some("llama".to_string()),
            ..Default::default()
        };
        metrics
            .get_hash_metrics("hash-a")
            .await
            .set_parsed_params(params, Some("b1234-abcdef".to_string()));

        let snapshot = metrics
            .snapshot("node-a".to_string(), "llama".to_string(), None)
            .await;
        // Through the exact persistence format (serde), as load() would see it.
        let json = serde_json::to_string(&snapshot).unwrap();
        let restored_snapshot: MetricsSnapshot = serde_json::from_str(&json).unwrap();
        let restored = Metrics::from_snapshot(restored_snapshot);

        let hm = restored.get_hash_metrics("hash-a").await;
        let params = hm
            .get_parsed_params(Some("b1234-abcdef"))
            .expect("parsed params should survive the snapshot round trip");
        assert_eq!(params.n_ctx, Some(4096));
        assert_eq!(params.n_ctx_train, Some(131072));
        assert_eq!(params.n_slots, Some(4));
        assert_eq!(params.arch.as_deref(), Some("llama"));
    }

    #[tokio::test]
    async fn test_parsed_params_withheld_across_llama_cpp_upgrade() {
        let hm = HashMetrics::default();
        let params = crate::instance::ParsedModelParams {
            n_ctx: Some(4096),
            ..Default::default()
        };
        hm.set_parsed_params(params, Some("b1000-old".to_string()));

        // Same binary: served.
        assert!(hm.get_parsed_params(Some("b1000-old")).is_some());
        // Different binary (same args hash): the same launch args can resolve
        // to different effective values on another build — withheld.
        assert!(hm.get_parsed_params(Some("b2000-new")).is_none());
        // Current version unknown (e.g. startup window before the build check
        // records it): nothing to compare against — served.
        assert!(hm.get_parsed_params(None).is_some());
    }

    #[tokio::test]
    async fn test_parsed_params_without_recorded_version_always_served() {
        // Params recorded when no llama.cpp version was known (managed builds
        // disabled) carry no version to compare; they are served as-is.
        let hm = HashMetrics::default();
        let params = crate::instance::ParsedModelParams {
            n_ctx: Some(2048),
            ..Default::default()
        };
        hm.set_parsed_params(params, None);

        assert!(hm.get_parsed_params(Some("b2000-new")).is_some());
        assert!(hm.get_parsed_params(None).is_some());
    }

    #[test]
    fn test_hash_snapshot_deserializes_without_parsed_params() {
        // Hash entries persisted by versions predating `parsed_model_params`
        // must still load (the field is `#[serde(default)]`).
        let legacy = r#"{
            "display_names": ["m:default"],
            "requests_total": 3,
            "errors_total": 0,
            "tokens_generated_total": 30,
            "avg_latency_ms": 10.0,
            "peak_vram_mb": 1000,
            "peak_sysmem_mb": 2000,
            "sample_count": 2
        }"#;
        let snapshot: HashMetricsSnapshot =
            serde_json::from_str(legacy).expect("legacy hash snapshot should deserialize");
        assert!(snapshot.parsed_model_params.is_none());
        assert_eq!(snapshot.requests_total, 3);
    }

    #[test]
    fn test_hash_snapshot_tolerates_malformed_parsed_params() {
        // A present-but-wrong-shape `parsed_model_params` (e.g. written by a
        // build where the field had a different layout) must NOT fail the
        // parse: that would discard the entire snapshot and silently reset
        // every persisted counter. The params are dropped, the counters kept.
        let drifted = r#"{
            "display_names": ["m:default"],
            "requests_total": 1428315,
            "errors_total": 16406,
            "tokens_generated_total": 30,
            "avg_latency_ms": 10.0,
            "peak_vram_mb": 1000,
            "peak_sysmem_mb": 2000,
            "sample_count": 2,
            "parsed_model_params": {"n_ctx": 4096, "n_slots": 4}
        }"#;
        let snapshot: HashMetricsSnapshot = serde_json::from_str(drifted)
            .expect("shape drift in parsed params must not discard the snapshot");
        assert!(snapshot.parsed_model_params.is_none());
        assert_eq!(snapshot.requests_total, 1428315);
        assert_eq!(snapshot.errors_total, 16406);
    }

    #[tokio::test]
    async fn test_tokens_per_second() {
        let hm = HashMetrics::default();
        hm.tokens_generated_total.fetch_add(1000, Ordering::Relaxed);
        hm.total_latency_ms.fetch_add(2000, Ordering::Relaxed);

        let tps = hm.tokens_per_second();
        assert!((tps - 500.0).abs() < 0.01); // 1000 tokens / 2 seconds = 500 TPS
    }

    #[tokio::test]
    async fn test_has_memory_data() {
        let metrics = Metrics::new();
        let args_hash = "test_hash";

        // Initially no data
        assert!(!metrics.has_memory_data(args_hash).await);

        // After observing memory
        let hm = metrics.get_hash_metrics(args_hash).await;
        hm.observe_memory(1000, 2000);

        assert!(metrics.has_memory_data(args_hash).await);
    }

    #[test]
    fn test_tokens_per_second_zero_latency() {
        let hm = HashMetrics::default();
        hm.tokens_generated_total.fetch_add(100, Ordering::Relaxed);
        // Zero latency should return 0, not panic or NaN
        assert_eq!(hm.tokens_per_second(), 0.0);
    }

    #[test]
    fn test_tokens_per_second_zero_tokens() {
        let hm = HashMetrics::default();
        hm.total_latency_ms.fetch_add(1000, Ordering::Relaxed);
        assert_eq!(hm.tokens_per_second(), 0.0);
    }

    #[test]
    fn test_latency_samples_capacity() {
        let hm = HashMetrics::default();
        // Add more than 100 samples to verify capacity management
        for i in 0..150 {
            hm.observe_latency(i as u64);
        }
        let samples = hm.latency_samples.lock();
        assert!(samples.len() <= 100);
        // Should contain recent samples (50-149), not first ones
        assert_eq!(*samples.front().unwrap(), 50);
        assert_eq!(*samples.back().unwrap(), 149);
    }

    #[test]
    fn test_observe_memory_peak_tracking() {
        let hm = HashMetrics::default();

        hm.observe_memory(100, 200);
        assert_eq!(hm.peak_vram_mb.load(Ordering::Relaxed), 100);
        assert_eq!(hm.peak_sysmem_mb.load(Ordering::Relaxed), 200);

        // Lower values shouldn't reduce peak
        hm.observe_memory(50, 100);
        assert_eq!(hm.peak_vram_mb.load(Ordering::Relaxed), 100);
        assert_eq!(hm.peak_sysmem_mb.load(Ordering::Relaxed), 200);

        // Higher values should increase peak
        hm.observe_memory(200, 300);
        assert_eq!(hm.peak_vram_mb.load(Ordering::Relaxed), 200);
        assert_eq!(hm.peak_sysmem_mb.load(Ordering::Relaxed), 300);
    }

    #[test]
    fn test_display_names_deduplication() {
        let hm = HashMetrics::default();
        hm.add_display_name("model:fast");
        hm.add_display_name("model:fast"); // Duplicate
        hm.add_display_name("model:slow");

        let names = hm.display_names.lock();
        assert_eq!(names.len(), 2);
    }

    #[test]
    fn test_escape_label_value_empty() {
        assert_eq!(escape_label_value(""), "");
    }

    #[test]
    fn test_escape_label_value_special_chars() {
        // Multiple special chars
        assert_eq!(escape_label_value("a\"b\\c\nd"), "a\\\"b\\\\c\\nd");
    }

    #[tokio::test]
    async fn test_metrics_current_requests_underflow() {
        let metrics = Metrics::new();
        // dec_current_requests with 0 should underflow to max u64
        // This documents current behavior - may want to use saturating_sub
        let before = metrics.current_requests.load(Ordering::Relaxed);
        metrics.dec_current_requests();
        let after = metrics.current_requests.load(Ordering::Relaxed);
        // Just verify it doesn't panic; actual behavior may vary
        assert!(after != before || before == 0);
    }
}
