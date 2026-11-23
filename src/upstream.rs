//! External upstreams: OpenAI-compatible endpoints that llamesh does not
//! start. A profile that names one is served by it, in place of a local
//! instance, for as long as its health checks pass.

use crate::config::{Cookbook, NodeConfig, UpstreamConfig};
use crate::util::error_chain;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{watch, OwnedSemaphorePermit, Semaphore};
use tracing::{debug, info, warn, Instrument};

/// Bounds connecting to an upstream, TLS handshake included, so that a
/// request to a host that has vanished moves on to another target.
const CONNECT_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a connection to an upstream stays in the pool unused. Kept short
/// for the reason given at `LOCAL_UPSTREAM_POOL_IDLE_TIMEOUT`: a request sent
/// on a connection the server is closing fails without a response.
const POOL_IDLE_TIMEOUT: Duration = Duration::from_secs(2);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Health {
    /// No health check has passed since the upstream was configured, or
    /// `failure_threshold` of them have failed in a row.
    Unhealthy,
    Healthy,
    /// The cookbook no longer has this upstream, or has changed it.
    Retired,
}

/// One profile's upstream and what this node knows about it.
pub struct Upstream {
    /// `model:profile`, as the cookbook spells it.
    pub key: String,
    pub config: UpstreamConfig,
    permits: Arc<Semaphore>,
    health: watch::Sender<Health>,
    waiting: AtomicUsize,
    /// Responses relayed from the upstream, whatever their status.
    pub requests_total: AtomicU64,
    /// Requests the upstream was sent that it gave no response to.
    pub failures_total: AtomicU64,
}

/// A request's place in line for an upstream, given up when dropped.
pub struct Waiting<'a>(&'a AtomicUsize);

impl Drop for Waiting<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
    }
}

impl Upstream {
    fn new(key: String, config: UpstreamConfig, default_concurrency: usize) -> Self {
        let permits = match config
            .max_concurrent_requests
            .unwrap_or(default_concurrency)
        {
            0 => Semaphore::MAX_PERMITS,
            limit => limit.min(Semaphore::MAX_PERMITS),
        };
        Self {
            key,
            config,
            permits: Arc::new(Semaphore::new(permits)),
            health: watch::Sender::new(Health::Unhealthy),
            waiting: AtomicUsize::new(0),
            requests_total: AtomicU64::new(0),
            failures_total: AtomicU64::new(0),
        }
    }

    pub fn is_healthy(&self) -> bool {
        *self.health.borrow() == Health::Healthy
    }

    /// Resolves once requests should no longer wait for this upstream: it is
    /// unhealthy, or the cookbook has dropped or changed it.
    pub async fn unavailable(&self) {
        self.health_is(|health| health != Health::Healthy).await;
    }

    /// Resolves once the upstream is unhealthy. A request it is working on is
    /// then given up; one sent to an upstream the cookbook has since dropped
    /// is left to finish.
    pub async fn unhealthy(&self) {
        self.health_is(|health| health == Health::Unhealthy).await;
    }

    async fn health_is(&self, matches: impl Fn(Health) -> bool) {
        // The sender is `self.health`, so the channel cannot close first.
        let _ = self
            .health
            .subscribe()
            .wait_for(|health| matches(*health))
            .await;
    }

    /// Record a health check's verdict. Returns whether it changed the state.
    fn set_healthy(&self, healthy: bool) -> bool {
        let new = if healthy {
            Health::Healthy
        } else {
            Health::Unhealthy
        };
        self.health.send_if_modified(|health| {
            let changes = *health != Health::Retired && *health != new;
            if changes {
                *health = new;
            }
            changes
        })
    }

    /// A turn at the upstream, if one is free now.
    pub fn try_turn(&self) -> Option<OwnedSemaphorePermit> {
        self.permits.clone().try_acquire_owned().ok()
    }

    /// A turn at the upstream, taken in the order requests asked.
    pub async fn turn(&self) -> OwnedSemaphorePermit {
        self.permits
            .clone()
            .acquire_owned()
            .await
            .expect("the upstream's semaphore is never closed")
    }

    /// Join the requests waiting for a turn. `None` when `limit` of them
    /// already wait; 0 is no limit.
    pub fn wait_in_line(&self, limit: usize) -> Option<Waiting<'_>> {
        self.waiting
            .fetch_update(Ordering::Relaxed, Ordering::Relaxed, |waiting| {
                (limit == 0 || waiting < limit).then_some(waiting + 1)
            })
            .ok()
            .map(|_| Waiting(&self.waiting))
    }
}

struct Entry {
    upstream: Arc<Upstream>,
    health_checks: tokio::task::AbortHandle,
}

/// The upstreams the cookbook names, by lowercase `model:profile`.
pub struct Upstreams {
    client: reqwest::Client,
    node_id: String,
    default_concurrency: usize,
    entries: parking_lot::RwLock<HashMap<String, Entry>>,
    active_requests: AtomicUsize,
}

fn entry_key(model: &str, profile_id: &str) -> String {
    format!("{}:{}", model.to_lowercase(), profile_id.to_lowercase())
}

impl Upstreams {
    pub fn new(config: &NodeConfig) -> reqwest::Result<Self> {
        let client = reqwest::Client::builder()
            .connect_timeout(CONNECT_TIMEOUT)
            .pool_idle_timeout(POOL_IDLE_TIMEOUT)
            // A request can wait a long time for its response with nothing
            // on the wire. Keepalive probes notice an upstream that has
            // vanished meanwhile.
            .tcp_keepalive(crate::cluster::PEER_TCP_KEEPALIVE_TIME)
            .tcp_keepalive_interval(crate::cluster::PEER_TCP_KEEPALIVE_INTERVAL)
            .tcp_keepalive_retries(crate::cluster::PEER_TCP_KEEPALIVE_RETRIES)
            // A redirect is the upstream's answer, relayed like any other.
            .redirect(reqwest::redirect::Policy::none())
            .build()?;
        Ok(Self {
            client,
            node_id: config.node_id.clone(),
            default_concurrency: config.model_defaults.max_concurrent_requests_per_instance,
            entries: parking_lot::RwLock::new(HashMap::new()),
            active_requests: AtomicUsize::new(0),
        })
    }

    pub fn client(&self) -> &reqwest::Client {
        &self.client
    }

    pub fn get(&self, model: &str, profile_id: &str) -> Option<Arc<Upstream>> {
        self.entries
            .read()
            .get(&entry_key(model, profile_id))
            .map(|entry| entry.upstream.clone())
    }

    /// Requests this node holds for an upstream: waiting for a turn, or sent
    /// and not yet answered in full. They use none of the node's own capacity.
    pub fn active_requests(&self) -> usize {
        self.active_requests.load(Ordering::Relaxed)
    }

    pub fn request_started(&self) {
        self.active_requests.fetch_add(1, Ordering::Relaxed);
    }

    pub fn request_finished(&self) {
        self.active_requests.fetch_sub(1, Ordering::Relaxed);
    }

    /// Match the upstreams to the cookbook. An upstream the cookbook still
    /// has unchanged keeps its health and its line of waiting requests. Any
    /// other is retired: requests waiting for it move on, and requests it is
    /// working on finish. New and changed upstreams start unhealthy and are
    /// checked at once. Must be called within the Tokio runtime.
    pub fn reconcile(&self, cookbook: &Cookbook) {
        let mut wanted = HashMap::new();
        for model in cookbook.models.iter().filter(|model| model.enabled) {
            for profile in model.profiles.iter().filter(|profile| profile.enabled) {
                if let Some(config) = &profile.upstream {
                    wanted.insert(
                        entry_key(&model.name, &profile.id),
                        (format!("{}:{}", model.name, profile.id), config),
                    );
                }
            }
        }

        let mut entries = self.entries.write();
        entries.retain(|key, entry| {
            let unchanged = wanted.get(key).is_some_and(|(name, config)| {
                *name == entry.upstream.key && **config == entry.upstream.config
            });
            if !unchanged {
                entry.health_checks.abort();
                entry.upstream.health.send_replace(Health::Retired);
            }
            unchanged
        });
        for (key, (name, config)) in wanted {
            if entries.contains_key(&key) {
                continue;
            }
            let upstream = Arc::new(Upstream::new(
                name,
                config.clone(),
                self.default_concurrency,
            ));
            let health_checks = tokio::spawn(
                check_health(upstream.clone(), self.client.clone())
                    .instrument(tracing::info_span!("node", node_id = %self.node_id)),
            )
            .abort_handle();
            entries.insert(
                key,
                Entry {
                    upstream,
                    health_checks,
                },
            );
        }
    }

    /// The upstream metrics in Prometheus text format; empty with no upstreams.
    pub fn render_prometheus(&self) -> String {
        let mut upstreams: Vec<Arc<Upstream>> = self
            .entries
            .read()
            .values()
            .map(|entry| entry.upstream.clone())
            .collect();
        upstreams.sort_by(|a, b| a.key.cmp(&b.key));

        let mut out = String::new();
        let mut family = |name: &str, help: &str, kind: &str, value: &dyn Fn(&Upstream) -> u64| {
            if upstreams.is_empty() {
                return;
            }
            out.push_str(&format!("# HELP {name} {help}\n# TYPE {name} {kind}\n"));
            for upstream in &upstreams {
                out.push_str(&format!(
                    "{name}{{model=\"{}\"}} {}\n",
                    crate::metrics::escape_label_value(&upstream.key),
                    value(upstream)
                ));
            }
        };
        family(
            "proxy_upstream_healthy",
            "Whether the profile's external upstream is healthy (1) or not (0).",
            "gauge",
            &|upstream| u64::from(upstream.is_healthy()),
        );
        family(
            "proxy_upstream_requests_total",
            "Responses relayed from the profile's external upstream.",
            "counter",
            &|upstream| upstream.requests_total.load(Ordering::Relaxed),
        );
        family(
            "proxy_upstream_failures_total",
            "Requests sent to the profile's external upstream that got no response.",
            "counter",
            &|upstream| upstream.failures_total.load(Ordering::Relaxed),
        );
        out
    }
}

impl Drop for Upstreams {
    fn drop(&mut self) {
        for entry in self.entries.get_mut().values() {
            entry.health_checks.abort();
        }
    }
}

/// Check an upstream's health until the task is aborted.
async fn check_health(upstream: Arc<Upstream>, client: reqwest::Client) {
    let check = &upstream.config.health_check;
    let url = upstream.config.endpoint(&check.path);
    let mut ticker = tokio::time::interval(Duration::from_millis(check.interval_ms));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut failures = 0u32;
    loop {
        ticker.tick().await;
        let mut request = client
            .get(&url)
            .timeout(Duration::from_millis(check.timeout_ms));
        if let Some(key) = &upstream.config.api_key {
            request = request.bearer_auth(key);
        }
        let failure = match request.send().await {
            Ok(response) if response.status().is_success() => None,
            Ok(response) => Some(format!("status {}", response.status())),
            Err(e) => Some(error_chain(&e)),
        };
        match failure {
            None => {
                failures = 0;
                if upstream.set_healthy(true) {
                    info!(
                        event = "upstream_healthy",
                        model = %upstream.key,
                        url = %upstream.config.url,
                        "External upstream is healthy"
                    );
                }
            }
            Some(error) => {
                failures = failures.saturating_add(1);
                // Logged once per outage, and when a newly configured
                // upstream fails its first checks.
                if failures == check.failure_threshold {
                    upstream.set_healthy(false);
                    warn!(
                        event = "upstream_unhealthy",
                        model = %upstream.key,
                        url = %upstream.config.url,
                        error = %error,
                        failed_checks = failures,
                        "External upstream is unhealthy"
                    );
                } else {
                    debug!(
                        model = %upstream.key,
                        error = %error,
                        failed_checks = failures,
                        "External upstream health check failed"
                    );
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicBool;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};
    use tokio::time::timeout;

    const SOON: Duration = Duration::from_secs(5);

    fn config(yaml: &str) -> UpstreamConfig {
        serde_yaml::from_str(yaml).unwrap()
    }

    fn upstream(yaml: &str) -> Upstream {
        Upstream::new("model:default".into(), config(yaml), 2)
    }

    fn cookbook(profiles: &str) -> Cookbook {
        serde_yaml::from_str(&format!(
            "models: [{{name: Model, profiles: [{profiles}]}}]"
        ))
        .unwrap()
    }

    fn registry() -> Upstreams {
        let node: NodeConfig = serde_json::from_value(serde_json::json!({
            "node_id": "node", "listen_addr": "127.0.0.1:0",
            "max_vram_mb": 1, "max_sysmem_mb": 1, "default_model": "unused",
            "model_defaults": {"max_instances_per_model": 1,
                "max_concurrent_requests_per_instance": 2, "max_queue_size_per_model": 1},
            "llama_cpp": {"repo_url": "", "branch": "master", "auto_update_interval_seconds": 0},
            "cluster": {"enabled": false, "gossip_interval_seconds": 5},
            "http": {"request_body_limit_bytes": 1024, "idle_timeout_seconds": 60}
        }))
        .unwrap();
        Upstreams::new(&node).unwrap()
    }

    /// An HTTP server whose every response has the status in `healthy`.
    async fn health_endpoint(healthy: Arc<AtomicBool>) -> std::net::SocketAddr {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            loop {
                let (mut stream, _) = listener.accept().await.unwrap();
                let healthy = healthy.clone();
                tokio::spawn(async move {
                    let mut request = [0u8; 1024];
                    let _ = stream.read(&mut request).await;
                    let status = if healthy.load(Ordering::Relaxed) {
                        "200 OK"
                    } else {
                        "503 Service Unavailable"
                    };
                    let response = format!(
                        "HTTP/1.1 {status}\r\ncontent-length: 0\r\nconnection: close\r\n\r\n"
                    );
                    let _ = stream.write_all(response.as_bytes()).await;
                });
            }
        });
        addr
    }

    async fn wait_for_health(upstream: &Upstream, healthy: bool) {
        timeout(SOON, async {
            while upstream.is_healthy() != healthy {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("upstream did not become healthy={healthy}"));
    }

    #[test]
    fn turns_are_limited_and_the_line_is_bounded() {
        let upstream = upstream("{url: 'http://h', max_concurrent_requests: 1}");
        let turn = upstream.try_turn().expect("the first request gets a turn");
        assert!(upstream.try_turn().is_none());

        let first = upstream.wait_in_line(2).expect("room in line");
        let second = upstream.wait_in_line(2).expect("room in line");
        assert!(upstream.wait_in_line(2).is_none(), "the line is full");
        assert!(upstream.wait_in_line(0).is_some(), "0 is no limit");
        drop((first, second));
        assert!(upstream.wait_in_line(1).is_some());

        drop(turn);
        assert!(upstream.try_turn().is_some());
    }

    #[test]
    fn concurrency_defaults_to_the_per_instance_limit_and_zero_is_unlimited() {
        let default = upstream("{url: 'http://h'}");
        let turns: Vec<_> = std::iter::from_fn(|| default.try_turn()).take(5).collect();
        assert_eq!(turns.len(), 2);

        let unlimited = upstream("{url: 'http://h', max_concurrent_requests: 0}");
        let turns: Vec<_> = std::iter::from_fn(|| unlimited.try_turn())
            .take(500)
            .collect();
        assert_eq!(turns.len(), 500);
    }

    #[tokio::test]
    async fn turns_are_taken_in_the_order_requests_asked() {
        let upstream = Arc::new(upstream("{url: 'http://h', max_concurrent_requests: 1}"));
        let held = upstream.try_turn().unwrap();
        let (served, mut order) = tokio::sync::mpsc::unbounded_channel();
        for request in 0..3 {
            let upstream = upstream.clone();
            let served = served.clone();
            tokio::spawn(async move {
                let _turn = upstream.turn().await;
                served.send(request).unwrap();
            });
            // Let the request join the line before the next one asks.
            tokio::task::yield_now().await;
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
        drop(held);
        for expected in 0..3 {
            assert_eq!(timeout(SOON, order.recv()).await.unwrap(), Some(expected));
        }
    }

    #[tokio::test]
    async fn waiters_and_sent_requests_see_different_health_changes() {
        let upstream = upstream("{url: 'http://h'}");
        assert!(!upstream.is_healthy(), "an unchecked upstream is not used");
        assert!(upstream.set_healthy(true));
        assert!(!upstream.set_healthy(true), "no change is not a change");

        let pending = Duration::from_millis(50);
        assert!(timeout(pending, upstream.unavailable()).await.is_err());
        assert!(timeout(pending, upstream.unhealthy()).await.is_err());

        // Retiring an upstream moves on the requests waiting for it, and
        // lets the requests it is working on finish.
        upstream.health.send_replace(Health::Retired);
        timeout(SOON, upstream.unavailable()).await.unwrap();
        assert!(timeout(pending, upstream.unhealthy()).await.is_err());
        assert!(
            !upstream.set_healthy(true),
            "a retired upstream stays retired"
        );
        assert!(!upstream.is_healthy());

        let failing = self::upstream("{url: 'http://h'}");
        failing.set_healthy(true);
        failing.set_healthy(false);
        timeout(SOON, failing.unavailable()).await.unwrap();
        timeout(SOON, failing.unhealthy()).await.unwrap();
    }

    #[tokio::test]
    async fn health_checks_follow_the_upstream() {
        let healthy = Arc::new(AtomicBool::new(true));
        let addr = health_endpoint(healthy.clone()).await;
        let upstreams = registry();
        upstreams.reconcile(&cookbook(&format!(
            "{{id: default, upstream: {{url: 'http://{addr}', health_check: \
             {{path: /health, interval_ms: 20, timeout_ms: 1000, failure_threshold: 2}}}}}}"
        )));
        let upstream = upstreams
            .get("model", "DEFAULT")
            .expect("lookup ignores case");
        assert_eq!(upstream.key, "Model:default");

        wait_for_health(&upstream, true).await;
        healthy.store(false, Ordering::Relaxed);
        wait_for_health(&upstream, false).await;
        healthy.store(true, Ordering::Relaxed);
        wait_for_health(&upstream, true).await;
    }

    #[tokio::test]
    async fn an_unreachable_upstream_stays_unhealthy() {
        // Bound and dropped: nothing listens on the port.
        let addr = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .unwrap()
            .local_addr()
            .unwrap();
        let upstreams = registry();
        upstreams.reconcile(&cookbook(&format!(
            "{{id: default, upstream: {{url: 'http://{addr}', health_check: {{interval_ms: 10}}}}}}"
        )));
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(!upstreams.get("Model", "default").unwrap().is_healthy());
    }

    #[tokio::test]
    async fn reconciling_keeps_unchanged_upstreams_and_retires_the_rest() {
        let upstreams = registry();
        let both = "{id: default, upstream: {url: 'http://127.0.0.1:9'}}, \
                    {id: other, model_path: /m.gguf, upstream: {url: 'http://127.0.0.1:9'}}";
        upstreams.reconcile(&cookbook(both));
        let kept = upstreams.get("Model", "default").unwrap();
        let changed = upstreams.get("Model", "other").unwrap();
        kept.set_healthy(true);
        changed.set_healthy(true);

        // `other` gets a new URL; a profile without an upstream is added.
        upstreams.reconcile(&cookbook(
            "{id: default, upstream: {url: 'http://127.0.0.1:9'}}, \
             {id: other, model_path: /m.gguf, upstream: {url: 'http://127.0.0.1:10'}}, \
             {id: plain, model_path: /m.gguf}",
        ));
        assert!(Arc::ptr_eq(
            &kept,
            &upstreams.get("Model", "default").unwrap()
        ));
        assert!(kept.is_healthy(), "an unchanged upstream keeps its health");
        let replacement = upstreams.get("Model", "other").unwrap();
        assert!(!Arc::ptr_eq(&changed, &replacement));
        assert!(
            !replacement.is_healthy(),
            "a changed upstream is checked afresh"
        );
        timeout(SOON, changed.unavailable()).await.unwrap();
        assert!(upstreams.get("Model", "plain").is_none());

        // A disabled profile's upstream is dropped like a removed one.
        upstreams.reconcile(&cookbook(
            "{id: default, enabled: false, upstream: {url: 'http://127.0.0.1:9'}}",
        ));
        assert!(upstreams.get("Model", "default").is_none());
        assert!(upstreams.get("Model", "other").is_none());
        timeout(SOON, kept.unavailable()).await.unwrap();
    }

    #[tokio::test]
    async fn metrics_list_each_upstream() {
        let upstreams = registry();
        assert_eq!(upstreams.render_prometheus(), "");

        upstreams.reconcile(&cookbook(
            "{id: default, upstream: {url: 'http://127.0.0.1:9'}}",
        ));
        let upstream = upstreams.get("Model", "default").unwrap();
        upstream.set_healthy(true);
        upstream.requests_total.store(7, Ordering::Relaxed);
        upstream.failures_total.store(2, Ordering::Relaxed);
        let rendered = upstreams.render_prometheus();
        for line in [
            "# TYPE proxy_upstream_healthy gauge",
            "proxy_upstream_healthy{model=\"Model:default\"} 1",
            "# TYPE proxy_upstream_requests_total counter",
            "proxy_upstream_requests_total{model=\"Model:default\"} 7",
            "proxy_upstream_failures_total{model=\"Model:default\"} 2",
        ] {
            assert!(
                rendered.lines().any(|l| l == line),
                "missing {line:?} in:\n{rendered}"
            );
        }
    }

    #[test]
    fn held_requests_are_counted() {
        let upstreams = registry();
        upstreams.request_started();
        upstreams.request_started();
        upstreams.request_finished();
        assert_eq!(upstreams.active_requests(), 1);
    }
}
