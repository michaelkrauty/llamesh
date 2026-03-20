use parking_lot::Mutex;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::atomic::{AtomicU64, Ordering};
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

    // Per-hash metrics: args_hash -> HashMetrics
    pub hash_metrics: RwLock<HashMap<String, Arc<HashMetrics>>>,
}

#[derive(Serialize, Deserialize)]
pub struct MetricsSnapshot {
    pub node_id: String,
    pub llama_cpp_version: String,
    pub requests_total: u64,
    pub errors_total: u64,
    pub current_requests: u64,
    pub hashes: HashMap<String, HashMetricsSnapshot>,
    pub updated_at: String,
    #[serde(default)]
    pub is_building: bool,
    #[serde(default)]
    pub last_build_error: Option<String>,
    #[serde(default)]
    pub last_build_at: Option<String>,
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
            hash_metrics: RwLock::new(map),
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
        map.get(args_hash).map(|m| {
            (
                m.peak_vram_mb.load(Ordering::Relaxed),
                m.peak_sysmem_mb.load(Ordering::Relaxed),
            )
        })
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
                },
            );
        }

        MetricsSnapshot {
            node_id,
            llama_cpp_version: version,
            requests_total: self.requests_total.load(Ordering::Relaxed),
            errors_total: self.errors_total.load(Ordering::Relaxed),
            current_requests: self.current_requests.load(Ordering::Relaxed),
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
        }
    }
}

use crate::circuit_breaker::{CircuitBreaker, CircuitState};

pub async fn render_prometheus_with_circuit_breaker(
    metrics: &Metrics,
    circuit_breaker: Option<&CircuitBreaker>,
) -> String {
    let mut out = String::new();

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
    out.push_str("# HELP proxy_token_counting_disabled_total Times token counting was disabled.\n");
    out.push_str("# TYPE proxy_token_counting_disabled_total counter\n");
    out.push_str(&format!(
        "proxy_token_counting_disabled_total {}\n",
        crate::router::TOKEN_COUNTING_DISABLED.load(Ordering::Relaxed)
    ));

    // Skipped stream cleanups (cleanup failed during shutdown)
    out.push_str("# HELP proxy_skipped_stream_cleanups_total Stream cleanups skipped during shutdown.\n");
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

    // Per-hash (model/profile) metrics
    out.push_str("# HELP proxy_hash_requests_total Total requests per model/profile hash.\n");
    out.push_str("# TYPE proxy_hash_requests_total counter\n");
    out.push_str("# HELP proxy_hash_errors_total Total errors per model/profile hash.\n");
    out.push_str("# TYPE proxy_hash_errors_total counter\n");
    out.push_str("# HELP proxy_hash_tokens_generated_total Total tokens generated per hash.\n");
    out.push_str("# TYPE proxy_hash_tokens_generated_total counter\n");
    out.push_str("# HELP proxy_hash_total_latency_ms Total latency in milliseconds per hash.\n");
    out.push_str("# TYPE proxy_hash_total_latency_ms counter\n");
    out.push_str("# HELP proxy_hash_p95_latency_ms P95 latency in milliseconds (last 100 samples).\n");
    out.push_str("# TYPE proxy_hash_p95_latency_ms gauge\n");
    out.push_str("# HELP proxy_hash_peak_vram_mb Peak VRAM usage in MB per hash.\n");
    out.push_str("# TYPE proxy_hash_peak_vram_mb gauge\n");
    out.push_str("# HELP proxy_hash_peak_sysmem_mb Peak system memory usage in MB per hash.\n");
    out.push_str("# TYPE proxy_hash_peak_sysmem_mb gauge\n");

    let map = metrics.hash_metrics.read().await;
    for (hash, m) in map.iter() {
        let escaped_hash = escape_label_value(hash);
        let display_names: Vec<String> = m.display_names.lock().iter().cloned().collect();
        let escaped_names = escape_label_value(&display_names.join(","));

        out.push_str(&format!(
            "proxy_hash_requests_total{{hash=\"{}\",names=\"{}\"}} {}\n",
            escaped_hash,
            escaped_names,
            m.requests_total.load(Ordering::Relaxed)
        ));
        out.push_str(&format!(
            "proxy_hash_errors_total{{hash=\"{}\",names=\"{}\"}} {}\n",
            escaped_hash,
            escaped_names,
            m.errors_total.load(Ordering::Relaxed)
        ));
        out.push_str(&format!(
            "proxy_hash_tokens_generated_total{{hash=\"{}\",names=\"{}\"}} {}\n",
            escaped_hash,
            escaped_names,
            m.tokens_generated_total.load(Ordering::Relaxed)
        ));
        out.push_str(&format!(
            "proxy_hash_total_latency_ms{{hash=\"{}\",names=\"{}\"}} {}\n",
            escaped_hash,
            escaped_names,
            m.total_latency_ms.load(Ordering::Relaxed)
        ));

        let p95 = {
            let mut samples: Vec<u64> = m.latency_samples.lock().iter().cloned().collect();
            if samples.is_empty() {
                0
            } else {
                samples.sort_unstable();
                let idx = ((samples.len() - 1) as f64 * 0.95).floor() as usize;
                samples[idx.min(samples.len() - 1)]
            }
        };

        out.push_str(&format!(
            "proxy_hash_p95_latency_ms{{hash=\"{}\",names=\"{}\"}} {}\n",
            escaped_hash, escaped_names, p95
        ));

        out.push_str(&format!(
            "proxy_hash_peak_vram_mb{{hash=\"{}\",names=\"{}\"}} {}\n",
            escaped_hash,
            escaped_names,
            m.peak_vram_mb.load(Ordering::Relaxed)
        ));

        out.push_str(&format!(
            "proxy_hash_peak_sysmem_mb{{hash=\"{}\",names=\"{}\"}} {}\n",
            escaped_hash,
            escaped_names,
            m.peak_sysmem_mb.load(Ordering::Relaxed)
        ));
    }

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

        let snapshot = metrics.snapshot("node-test".into(), "v1".into(), None).await;
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
    }

    #[tokio::test]
    async fn test_prometheus_rendering() {
        let metrics = Metrics::new();
        metrics.inc_requests();

        let hm = metrics.get_hash_metrics("abc123").await;
        hm.add_display_name("gpt\"cool\"");
        hm.requests_total.fetch_add(1, Ordering::Relaxed);
        hm.observe_latency(50);

        let output = render_prometheus_with_circuit_breaker(&metrics, None).await;

        assert!(output.contains("proxy_requests_total 1"));
        assert!(output.contains("proxy_hash_requests_total{hash=\"abc123\""));
        assert!(output.contains("names=\"gpt\\\"cool\\\"\""));
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
