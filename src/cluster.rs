use crate::node_state::NodeState;
use crate::node_state::PeerState;
use crate::security::PeerIdentity;
use axum::{
    extract::{ConnectInfo, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    Json,
};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Semaphore;
use tracing::{debug, info};

/// Per-request timeout for a single gossip exchange with a peer, applied on
/// both the Noise and plaintext transports. Bounds each request so an
/// unresponsive peer that accepts the connection but never replies cannot hold
/// a gossip task (and its concurrency permit) open indefinitely.
const GOSSIP_REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(10);

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerInfo {
    pub node_id: String,
    pub address: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct GossipMessage {
    pub origin: PeerState,
    pub known_peers: Vec<PeerInfo>,
}

pub async fn start_gossip_loop(state: Arc<NodeState>) {
    if !state.config.cluster.enabled {
        return;
    }

    info!("Starting cluster gossip loop");
    let period = std::time::Duration::from_secs(state.config.cluster.gossip_interval_seconds);

    let client = state.cluster_client.clone();

    // Semaphore to limit concurrent gossip tasks
    let semaphore = Arc::new(Semaphore::new(state.config.cluster.max_concurrent_gossip));

    // Circuit breaker is now used for peer health tracking (via state.circuit_breaker)

    // Use tokio::time::interval so the first tick fires immediately (no initial sleep),
    // then subsequent ticks wait `gossip_interval_seconds`.
    let mut interval = tokio::time::interval(period);
    let gossip_trigger = state.gossip_trigger.clone();

    loop {
        // Wait for either the periodic interval or an instant gossip trigger
        // (fired when significant state changes occur: instance ready/stopped, drain, etc.)
        tokio::select! {
            _ = interval.tick() => {}
            _ = gossip_trigger.notified() => {}
        }
        let my_state = state.get_self_peer_state().await;

        // Collect all targets: seeds + mDNS-discovered + known peers
        let mut targets = std::collections::HashSet::new();
        for seed in &state.config.cluster.peers {
            targets.insert(seed.clone());
        }

        // Add mDNS-discovered peers
        if let Some(ref discovery) = state.discovery {
            for addr in discovery.get_peer_addrs() {
                // mDNS gives us "ip:port", need to add scheme
                let url = if addr.contains("://") {
                    addr
                } else {
                    format!("http://{addr}")
                };
                targets.insert(url);
            }
        }

        let mut known_peers_info = Vec::new();
        // Map URL -> node_id so circuit breaker keys match routing (which uses node_id)
        let mut url_to_node_id: std::collections::HashMap<String, String> =
            std::collections::HashMap::new();

        {
            let peers = state.peers.read().await;
            for peer in peers.values() {
                // peer.address might be the base URL, e.g. "http://node-b:8080"
                targets.insert(peer.address.clone());
                url_to_node_id.insert(peer.address.clone(), peer.node_id.clone());

                known_peers_info.push(PeerInfo {
                    node_id: peer.node_id.clone(),
                    address: peer.address.clone(),
                });
            }
        }

        // Gossip to all targets
        for peer_url in targets {
            // Don't gossip to self
            if peer_url == my_state.address {
                continue;
            }

            debug!("Gossiping to peer {}", peer_url);
            let url = format!("{}/cluster/gossip", peer_url.trim_end_matches('/'));
            let client = client.clone();
            let noise_context = state.noise_context.clone();

            let message = GossipMessage {
                origin: my_state.clone(),
                known_peers: known_peers_info.clone(),
            };

            // Acquire semaphore permit to limit concurrent tasks (with timeout)
            let permit = match tokio::time::timeout(
                std::time::Duration::from_secs(5),
                semaphore.clone().acquire_owned(),
            )
            .await
            {
                Ok(Ok(permit)) => permit,
                Ok(Err(_)) => continue, // Semaphore closed (shouldn't happen)
                Err(_) => {
                    debug!("Gossip to {} skipped: semaphore timeout", peer_url);
                    continue; // Skip this peer this round
                }
            };
            // Use node_id as circuit breaker key to match routing code,
            // falling back to URL for seed peers we haven't heard from yet
            let cb_key = url_to_node_id
                .get(&peer_url)
                .cloned()
                .unwrap_or_else(|| peer_url.clone());
            let expected_node_id = url_to_node_id.get(&peer_url).cloned();
            let circuit_breaker = state.circuit_breaker.clone();
            let capacity_notify = state.capacity_notify.clone();
            // Gossip always sends regardless of circuit state — it is the
            // cluster's standing health probe. Claim the recovery probe slot
            // when one is available so a gossip success while the circuit is
            // open is attributed as a recovery probe result; without traffic,
            // this is what closes the circuit after a peer comes back. When
            // the claim is denied (a routed request's probe is in flight),
            // this round's result carries no ticket and cannot drive
            // recovery, but closed-circuit failure counting still works.
            let gossip_probe_ticket = matches!(
                circuit_breaker.try_claim_dispatch_sync(&cb_key),
                crate::circuit_breaker::DispatchDecision::Admit { probe_ticket: true }
            );

            // Spawn each gossip request to avoid head-of-line blocking in the loop
            tokio::spawn(async move {
                let _permit = permit; // Hold permit until task completes
                if let Some(noise_context) = noise_context {
                    let mut headers = HeaderMap::new();
                    headers.insert(
                        axum::http::header::CONTENT_TYPE,
                        HeaderValue::from_static("application/json"),
                    );
                    match serde_json::to_vec(&message) {
                        Ok(body) => {
                            match crate::noise::transport::request(
                                &noise_context,
                                crate::noise::transport::OutboundNoiseRequest {
                                    peer_base_url: &peer_url,
                                    expected_peer_node_id: expected_node_id.as_deref(),
                                    method: "POST",
                                    path: "/cluster/gossip",
                                    headers: &headers,
                                    body: &body,
                                    timeout_duration: Some(GOSSIP_REQUEST_TIMEOUT),
                                },
                            )
                            .await
                            {
                                Ok(resp) => {
                                    let status = resp.head.status;
                                    // Bound the body drain by the same gossip timeout so a
                                    // peer that stalls mid-body cannot hang the gossip loop.
                                    let mut body =
                                        resp.into_body_stream(Some(GOSSIP_REQUEST_TIMEOUT));
                                    let mut drained = true;
                                    while let Some(chunk) = body.next().await {
                                        if let Err(e) = chunk {
                                            debug!(
                                                "Failed to drain Noise gossip response from {}: {}",
                                                peer_url, e
                                            );
                                            drained = false;
                                            break;
                                        }
                                    }
                                    // A 2xx head whose body could not be drained (e.g. the peer
                                    // stalled and tripped the read timeout) is a failed exchange,
                                    // not a healthy one — record it as such instead of success.
                                    if (200..300).contains(&status) && drained {
                                        if circuit_breaker
                                            .record_success(&cb_key, gossip_probe_ticket)
                                            .await
                                        {
                                            capacity_notify.notify_waiters();
                                        }
                                    } else {
                                        circuit_breaker
                                            .record_failure(&cb_key, gossip_probe_ticket)
                                            .await;
                                        debug!(
                                            "Failed to gossip to {} over Noise: HTTP {} (body drained: {})",
                                            peer_url, status, drained
                                        );
                                    }
                                }
                                Err(e) => {
                                    circuit_breaker
                                        .record_failure(&cb_key, gossip_probe_ticket)
                                        .await;
                                    debug!("Failed to gossip to {} over Noise: {}", peer_url, e);
                                }
                            }
                        }
                        Err(e) => {
                            circuit_breaker
                                .record_failure(&cb_key, gossip_probe_ticket)
                                .await;
                            debug!("Failed to serialize gossip for {}: {}", peer_url, e);
                        }
                    }
                } else {
                    match client
                        .post(&url)
                        .json(&message)
                        .timeout(GOSSIP_REQUEST_TIMEOUT)
                        .send()
                        .await
                    {
                        Ok(resp) if resp.status().is_success() => {
                            // Record success - resets failure count, and on a
                            // recovery transition wakes parked routing waiters.
                            if circuit_breaker
                                .record_success(&cb_key, gossip_probe_ticket)
                                .await
                            {
                                capacity_notify.notify_waiters();
                            }
                        }
                        Ok(resp) => {
                            circuit_breaker
                                .record_failure(&cb_key, gossip_probe_ticket)
                                .await;
                            debug!("Failed to gossip to {}: HTTP {}", url, resp.status());
                        }
                        Err(e) => {
                            // Record failure - circuit breaker handles escalation and logging
                            circuit_breaker
                                .record_failure(&cb_key, gossip_probe_ticket)
                                .await;
                            debug!("Failed to gossip to {}: {}", url, e);
                        }
                    }
                }
            });
        }
    }
}

pub async fn handle_gossip(
    State(state): State<Arc<NodeState>>,
    maybe_identity: Option<ConnectInfo<PeerIdentity>>,
    maybe_socket: Option<ConnectInfo<std::net::SocketAddr>>,
    Json(msg): Json<GossipMessage>,
) -> Result<Json<serde_json::Value>, (StatusCode, String)> {
    let cluster_tls_enabled = state
        .config
        .cluster_tls
        .as_ref()
        .map(|c| c.enabled)
        .unwrap_or(false);
    if state.config.cluster.noise.enabled && !cluster_tls_enabled {
        return Err((
            StatusCode::UNAUTHORIZED,
            "Noise transport required for gossip".into(),
        ));
    }

    if state
        .config
        .cluster_tls
        .as_ref()
        .map(|c| c.enabled)
        .unwrap_or(false)
    {
        match maybe_identity {
            Some(ConnectInfo(identity)) => {
                if !identity.authenticated {
                    return Err((
                        StatusCode::UNAUTHORIZED,
                        "Client certificate required".into(),
                    ));
                }
                if let Some(cn) = identity.node_id {
                    if cn != msg.origin.node_id {
                        return Err((
                            StatusCode::FORBIDDEN,
                            format!(
                                "Certificate CN '{}' does not match Node ID '{}'",
                                cn, msg.origin.node_id
                            ),
                        ));
                    }
                } else {
                    return Err((
                        StatusCode::UNAUTHORIZED,
                        "Client certificate missing CN".into(),
                    ));
                }
            }
            None => {
                // Missing ConnectInfo implies connection didn't go through our TLS wrapper
                return Err((
                    StatusCode::UNAUTHORIZED,
                    "Mutual TLS required for gossip".into(),
                ));
            }
        }
    }

    // Extract the source address from the socket if available
    let source_addr = maybe_socket.map(|ConnectInfo(addr)| addr);
    process_gossip_message(&state, msg, source_addr).await;
    Ok(Json(serde_json::json!({"status": "ok"})))
}

/// Extract the host portion from a URL (e.g., "http://node-b:8080" -> "node-b")
fn extract_host_from_url(url: &str) -> &str {
    let after_scheme = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url);

    // Handle IPv6 addresses in brackets
    if after_scheme.starts_with('[') {
        after_scheme
            .find(']')
            .map(|idx| &after_scheme[1..idx])
            .unwrap_or(after_scheme)
    } else {
        // Regular host:port format - get everything before the colon
        after_scheme.split(':').next().unwrap_or(after_scheme)
    }
}

/// Check if an address URL contains a loopback address (127.x.x.x or localhost)
fn is_loopback_address(url: &str) -> bool {
    // Extract the host portion from URLs like "http://127.0.0.1:8080" or "http://localhost:8080"
    let after_scheme = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url);

    // Handle IPv6 addresses in brackets like [::1]:8080
    let host = if after_scheme.starts_with('[') {
        // Find the closing bracket
        after_scheme
            .find(']')
            .map(|idx| &after_scheme[..=idx])
            .unwrap_or(after_scheme)
    } else {
        // Regular host:port format
        after_scheme.split(':').next().unwrap_or("")
    };

    host == "localhost"
        || host == "127.0.0.1"
        || host.starts_with("127.")
        || host == "::1"
        || host == "[::1]"
}

/// Compare configured URLs without guessing DNS aliases or dropping meaningful
/// scheme, port, or path differences. Bare peer addresses imply HTTP.
fn same_peer_url(left: &str, right: &str) -> bool {
    matches!((parse_peer_url(left), parse_peer_url(right)), (Some(left), Some(right)) if left == right)
}

fn parse_peer_url(address: &str) -> Option<reqwest::Url> {
    let url = reqwest::Url::parse(&if address.contains("://") {
        address.to_string()
    } else {
        format!("http://{address}")
    })
    .ok()?;
    matches!(url.scheme(), "http" | "https").then_some(url)
}

fn source_peer_url(advertised: &str, source: std::net::SocketAddr) -> Option<String> {
    // Dual-stack sockets can report IPv4 sources as mapped IPv6 addresses.
    let ip = source.ip().to_canonical();
    if ip.is_unspecified() || ip.is_loopback() || ip.is_multicast() {
        return None;
    }
    if matches!(ip, std::net::IpAddr::V4(ip) if ip.is_broadcast()) {
        return None;
    }
    // URL transports cannot retain a link-local IPv6 interface scope.
    if matches!(ip, std::net::IpAddr::V6(ip) if ip.is_unicast_link_local()) {
        return None;
    }
    let advertised = parse_peer_url(advertised)?;
    let port = advertised
        .port_or_known_default()
        .filter(|port| *port != 0)?;
    let endpoint = std::net::SocketAddr::new(ip, port);
    Some(format!("{}://{endpoint}", advertised.scheme()))
}

/// Decide whether a peer version mismatch should be logged now, recording the
/// version when it should. Returns `true` only when the peer's mismatched
/// version is newly seen or has changed since the last time we logged about that
/// peer — edge-triggering the log so a persistent skew is reported once per
/// distinct version rather than on every gossip round.
fn note_version_mismatch_for_logging(
    logged: &mut std::collections::HashMap<String, String>,
    node_id: &str,
    peer_version: &str,
) -> bool {
    if logged.get(node_id).map(String::as_str) == Some(peer_version) {
        false
    } else {
        logged.insert(node_id.to_string(), peer_version.to_string());
        true
    }
}

pub async fn process_gossip_message(
    state: &NodeState,
    msg: GossipMessage,
    source_addr: Option<std::net::SocketAddr>,
) {
    let mut peers = state.peers.write().await;
    let mut peer_state = msg.origin;
    // Provenance belongs to this receiver, never to the incoming advertisement.
    peer_state.address_from_source = false;

    let local_version = env!("CARGO_PKG_VERSION");
    if peer_state.version != local_version {
        let action = &state.config.cluster.version_mismatch_action;

        // Extract major version for comparison
        let local_major = local_version.split('.').next().unwrap_or("0");
        let peer_major = peer_state.version.split('.').next().unwrap_or("0");

        let should_reject = match action.as_str() {
            "reject_any" => true,
            "reject_major" => local_major != peer_major,
            _ => false, // "warn" or any other value
        };

        // Edge-trigger the mismatch log. This handler runs on every inbound
        // gossip (once per `gossip_interval_seconds` per peer), so logging on
        // each round floods the log for the entire duration of a version skew
        // (e.g. a rolling upgrade). Only log when this peer's version is first
        // seen or changes.
        //
        // LOCK_ORDER: `logged_peer_versions` is a synchronous leaf mutex held
        // only for this map access and never across `.await`, so it is outside
        // the async lock ordering documented on `NodeState`.
        let should_log = {
            let mut logged = state.logged_peer_versions.lock().unwrap();
            note_version_mismatch_for_logging(&mut logged, &peer_state.node_id, &peer_state.version)
        };

        if should_log {
            if should_reject {
                tracing::warn!(
                    event = "peer_rejected_version_mismatch",
                    peer_node_id = %peer_state.node_id,
                    peer_version = %peer_state.version,
                    local_version = %local_version,
                    action = %action,
                    "Rejecting peer due to version mismatch"
                );
            } else {
                tracing::warn!(
                    "Version mismatch detected: Peer {} is on version {}, but local node is on version {}",
                    peer_state.node_id,
                    peer_state.version,
                    local_version
                );
            }
        }

        if should_reject {
            return; // Don't process this gossip message
        }
    } else {
        // Versions match: forget any remembered skew so that if this peer later
        // diverges again it is logged afresh, and the map does not retain stale
        // entries for peers that have caught up.
        state
            .logged_peer_versions
            .lock()
            .unwrap()
            .remove(&peer_state.node_id);
    }

    // Update the origin peer with local timestamp
    peer_state.last_seen = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);

    // Handle loopback address resolution
    // This handles the case where a peer has public_url unset and gossips 127.0.0.1
    if is_loopback_address(&peer_state.address) {
        let mut resolved = false;

        // Preserve authoritative/configured routes, but allow a locally inferred
        // endpoint to follow subsequent direct gossip after an address change.
        if let Some(existing) = peers.get(&peer_state.node_id) {
            if !is_loopback_address(&existing.address)
                && (!existing.address_from_source
                    || state
                        .config
                        .cluster
                        .peers
                        .iter()
                        .any(|seed| same_peer_url(seed, &existing.address)))
            {
                debug!(
                    "Preserving existing address {} for peer {} (gossiped address {} is loopback)",
                    existing.address, peer_state.node_id, peer_state.address
                );
                peer_state.address = existing.address.clone();
                resolved = true;
            }
        }

        // 2. Check if this peer matches one of our configured seed peers
        if !resolved {
            for seed_url in &state.config.cluster.peers {
                let seed_host = extract_host_from_url(seed_url);
                if seed_host == peer_state.node_id && !is_loopback_address(seed_url) {
                    debug!(
                        "Using seed peer address {} for peer {} (gossiped address {} is loopback)",
                        seed_url, peer_state.node_id, peer_state.address
                    );
                    peer_state.address = seed_url.clone();
                    resolved = true;
                    break;
                }
            }
        }

        // Refresh only inferred endpoints. The peer advertises its listener
        // port; the TCP source port is ephemeral and must never be routed to.
        if !resolved {
            let existing = peers.get(&peer_state.node_id);
            if let Some(derived_address) =
                source_addr.and_then(|source| source_peer_url(&peer_state.address, source))
            {
                if existing.map(|peer| &peer.address) != Some(&derived_address) {
                    info!(
                        "Using derived address {} for peer {} from source socket (gossiped address {} is loopback)",
                        derived_address, peer_state.node_id, peer_state.address
                    );
                }
                peer_state.address = derived_address;
                peer_state.address_from_source = true;
            } else if let Some(existing) = existing {
                // Missing/unusable source information must not erase a working
                // fallback or make it authoritative for the next update.
                peer_state.address = existing.address.clone();
                peer_state.address_from_source = existing.address_from_source;
            }
        }
    }

    peers.insert(peer_state.node_id.clone(), peer_state);

    // Process transitive peers
    let my_id = &state.config.node_id;

    for info in msg.known_peers {
        if info.node_id == *my_id {
            continue;
        }

        // If we don't know this peer, insert a placeholder so we start gossiping to it
        if !peers.contains_key(&info.node_id) {
            info!(
                "Discovered new peer {} at {} via gossip",
                info.node_id, info.address
            );
            // Use current timestamp to prevent immediate timeout of placeholder peers.
            // Before this fix, last_seen: 0 would cause `now - 0 > timeout` to always be true.
            let now_secs = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0);
            peers.insert(
                info.node_id.clone(),
                PeerState {
                    node_id: info.node_id,
                    address: info.address,
                    address_from_source: false,
                    version: "unknown".to_string(),
                    // Not yet heard from directly; filled in on first gossip.
                    llama_cpp_version: "unknown".to_string(),
                    last_seen: now_secs,
                    // 0 until the peer gossips its own start time directly.
                    started_at_unix: 0,
                    supported_models: vec![],
                    active_instances: 0,
                    max_instances: 0,
                    current_requests: 0,
                    available_vram: 0,
                    available_sysmem: 0,
                    llamesh_vram_mb: 0,
                    llamesh_sysmem_mb: 0,
                    external_vram_mb: 0,
                    device_vram_used_mb: 0,
                    device_vram_total_mb: 0,
                    gpu_telemetry_available: false,
                    max_vram: 0,
                    max_sysmem: 0,
                    total_queue_length: 0,
                    model_stats: std::collections::HashMap::new(),
                    ready: false, // Unknown until we hear directly from this peer
                    loaded_models: vec![],
                },
            );
        }
    }

    // Release lock before notifying to avoid holding it during wakeup
    drop(peers);

    // Wake cluster-aware waiters - peer state may have changed
    // This allows route_or_wait() to re-check if any node now has capacity
    state.capacity_notify.notify_waiters();
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build_manager::BuildManager;
    use crate::config::{
        ClusterConfig, Cookbook, HttpConfig, LlamaCppConfig, ModelDefaults, NodeConfig,
    };

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
            model_defaults: ModelDefaults {
                max_concurrent_requests_per_instance: 1,
                max_queue_size_per_model: 1,
                max_instances_per_model: 1,
                max_wait_in_queue_ms: 500,
                max_request_duration_ms: 300_000,
                min_eviction_tenure_secs: 15,
            },
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
                peers: vec![],
                gossip_interval_seconds: 5,
                max_concurrent_gossip: 16,
                discovery: crate::config::DiscoveryConfig {
                    mdns: false,
                    ..Default::default()
                },
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
            upstream_read_timeout_ms: 600_000,
            wedge_detector: crate::config::WedgeDetectorConfig::default(),
        }
    }

    /// Build a `PeerState` for a peer at a given version with otherwise
    /// unremarkable fields, for exercising version-skew handling.
    fn peer_state_with_version(node_id: &str, version: &str) -> PeerState {
        PeerState {
            node_id: node_id.to_string(),
            address: format!("http://{node_id}"),
            address_from_source: false,
            version: version.to_string(),
            llama_cpp_version: "unknown".to_string(),
            last_seen: 1000,
            started_at_unix: 0,
            supported_models: vec![],
            active_instances: 0,
            max_instances: 8,
            current_requests: 0,
            available_vram: 1,
            available_sysmem: 1,
            llamesh_vram_mb: 0,
            llamesh_sysmem_mb: 0,
            external_vram_mb: 0,
            device_vram_used_mb: 0,
            device_vram_total_mb: 0,
            gpu_telemetry_available: false,
            max_vram: 16000,
            max_sysmem: 64000,
            total_queue_length: 0,
            model_stats: std::collections::HashMap::new(),
            ready: true,
            loaded_models: vec![],
        }
    }

    #[test]
    fn test_note_version_mismatch_logs_once_per_distinct_version() {
        let mut logged = std::collections::HashMap::new();

        // First sight of a skewed version logs.
        assert!(note_version_mismatch_for_logging(
            &mut logged,
            "peer-a",
            "1.5.0"
        ));
        // The same version on subsequent gossip rounds does not log again.
        assert!(!note_version_mismatch_for_logging(
            &mut logged,
            "peer-a",
            "1.5.0"
        ));
        assert!(!note_version_mismatch_for_logging(
            &mut logged,
            "peer-a",
            "1.5.0"
        ));
        // A changed (still-skewed) version logs once for the new value.
        assert!(note_version_mismatch_for_logging(
            &mut logged,
            "peer-a",
            "1.5.1"
        ));
        assert!(!note_version_mismatch_for_logging(
            &mut logged,
            "peer-a",
            "1.5.1"
        ));
        // Other peers are tracked independently.
        assert!(note_version_mismatch_for_logging(
            &mut logged,
            "peer-b",
            "1.5.1"
        ));
        assert!(!note_version_mismatch_for_logging(
            &mut logged,
            "peer-b",
            "1.5.1"
        ));
    }

    #[tokio::test]
    async fn test_gossip_records_then_clears_version_mismatch() {
        let config = minimal_node_config();
        let cookbook = Cookbook { models: vec![] };
        let build_manager = BuildManager::new(config.llama_cpp.clone());
        let state = NodeState::new(config, cookbook, build_manager)
            .await
            .unwrap();

        let local = env!("CARGO_PKG_VERSION");

        // A peer on a different version is recorded so the skew is logged only
        // once instead of on every gossip round.
        let skewed = peer_state_with_version("peer-skew", "0.0.1");
        process_gossip_message(
            &state,
            GossipMessage {
                origin: skewed,
                known_peers: vec![],
            },
            None,
        )
        .await;
        assert_eq!(
            state
                .logged_peer_versions
                .lock()
                .unwrap()
                .get("peer-skew")
                .map(String::as_str),
            Some("0.0.1")
        );

        // When that peer reports the local version, the dedup record is cleared
        // so a later divergence is logged afresh.
        let matched = peer_state_with_version("peer-skew", local);
        process_gossip_message(
            &state,
            GossipMessage {
                origin: matched,
                known_peers: vec![],
            },
            None,
        )
        .await;
        assert!(state
            .logged_peer_versions
            .lock()
            .unwrap()
            .get("peer-skew")
            .is_none());
    }

    #[tokio::test]
    async fn test_gossip_discovers_new_peers() {
        let config = minimal_node_config();
        let cookbook = Cookbook { models: vec![] };
        let build_manager = BuildManager::new(config.llama_cpp.clone());
        let state = NodeState::new(config, cookbook, build_manager)
            .await
            .unwrap();

        let origin_peer = PeerState {
            node_id: "node-origin".into(),
            address: "http://node-origin".into(),
            address_from_source: false,
            version: "0.1.0".into(),
            llama_cpp_version: "origincpp1".into(),
            last_seen: 1000,
            started_at_unix: 0,
            supported_models: vec![],
            active_instances: 0,
            max_instances: 8,
            current_requests: 0,
            available_vram: 1,
            available_sysmem: 1,
            llamesh_vram_mb: 0,
            llamesh_sysmem_mb: 0,
            external_vram_mb: 0,
            device_vram_used_mb: 0,
            device_vram_total_mb: 0,
            gpu_telemetry_available: false,
            max_vram: 16000,
            max_sysmem: 64000,
            total_queue_length: 0,
            model_stats: std::collections::HashMap::new(),
            ready: true,
            loaded_models: vec![],
        };

        let new_peer_info = PeerInfo {
            node_id: "node-new".into(),
            address: "http://node-new".into(),
        };

        let msg = GossipMessage {
            origin: origin_peer.clone(),
            known_peers: vec![new_peer_info.clone()],
        };

        process_gossip_message(&state, msg, None).await;

        let peers = state.peers.read().await;

        // Should have origin peer, including the llama.cpp version it gossiped
        // (so /cluster/nodes can surface per-node llama.cpp version skew).
        assert!(peers.contains_key("node-origin"));
        let origin = peers.get("node-origin").unwrap();
        assert_eq!(origin.llama_cpp_version, "origincpp1");

        // Should have discovered peer. Transitively-discovered peers have no
        // direct gossip yet, so their llama.cpp version is "unknown" until heard.
        assert!(peers.contains_key("node-new"));
        let new_peer = peers.get("node-new").unwrap();
        assert_eq!(new_peer.address, "http://node-new");
        assert_eq!(new_peer.version, "unknown");
        assert_eq!(new_peer.llama_cpp_version, "unknown");
    }

    #[test]
    fn test_is_loopback_address() {
        // Loopback addresses
        assert!(is_loopback_address("http://127.0.0.1:8080"));
        assert!(is_loopback_address("http://127.0.0.1"));
        assert!(is_loopback_address("https://127.0.0.1:443"));
        assert!(is_loopback_address("http://localhost:8080"));
        assert!(is_loopback_address("http://localhost"));
        assert!(is_loopback_address("http://127.0.1.1:8080"));
        assert!(is_loopback_address("http://[::1]:8080"));
        assert!(is_loopback_address("http://[::1]"));

        // Non-loopback addresses
        assert!(!is_loopback_address("http://192.168.1.1:8080"));
        assert!(!is_loopback_address("http://node-a.example.com:8080"));
        assert!(!is_loopback_address("http://node-b:8080"));
        assert!(!is_loopback_address("https://10.0.0.1:443"));
    }

    #[tokio::test]
    async fn test_gossip_source_address_fallback_supports_both_ip_families() {
        let mut config = minimal_node_config();
        config.cluster.noise.enabled = false;
        let build_manager = BuildManager::new(config.llama_cpp.clone());
        let state = NodeState::new(config, Cookbook { models: vec![] }, build_manager)
            .await
            .unwrap();

        for (index, (advertised, source, expected)) in [
            (
                "http://127.0.0.1:8080",
                "192.0.2.1:50000",
                "http://192.0.2.1:8080",
            ),
            (
                "http://[::1]:9000",
                "[2001:db8::1]:50000",
                "http://[2001:db8::1]:9000",
            ),
            (
                "https://127.0.0.1:8443",
                "[2001:db8::2]:50000",
                "https://[2001:db8::2]:8443",
            ),
        ]
        .into_iter()
        .enumerate()
        {
            let node_id = format!("peer-{index}");
            let mut origin = peer_state_with_version(&node_id, env!("CARGO_PKG_VERSION"));
            origin.address = advertised.into();
            process_gossip_message(
                &state,
                GossipMessage {
                    origin,
                    known_peers: vec![],
                },
                Some(source.parse().unwrap()),
            )
            .await;

            let peers = state.peers.read().await;
            let address = &peers.get(&node_id).unwrap().address;
            assert_eq!(address, expected);
            assert!(reqwest::Url::parse(address).is_ok());
        }
    }

    #[tokio::test]
    async fn test_gossip_preserves_non_loopback_address() {
        let config = minimal_node_config();
        let cookbook = Cookbook { models: vec![] };
        let build_manager = BuildManager::new(config.llama_cpp.clone());
        let state = NodeState::new(config, cookbook, build_manager)
            .await
            .unwrap();

        // First, add a peer with a real address
        {
            let mut peers = state.peers.write().await;
            peers.insert(
                "remote-peer".into(),
                PeerState {
                    node_id: "remote-peer".into(),
                    address: "http://node-b:8080".into(), // Real hostname
                    address_from_source: false,
                    version: "0.1.0".into(),
                    llama_cpp_version: "unknown".into(),
                    last_seen: 1000,
                    started_at_unix: 0,
                    supported_models: vec!["model:default".into()],
                    active_instances: 0,
                    max_instances: 8,
                    current_requests: 0,
                    available_vram: 1000,
                    available_sysmem: 1000,
                    llamesh_vram_mb: 0,
                    llamesh_sysmem_mb: 0,
                    external_vram_mb: 0,
                    device_vram_used_mb: 0,
                    device_vram_total_mb: 0,
                    gpu_telemetry_available: false,
                    max_vram: 16000,
                    max_sysmem: 64000,
                    total_queue_length: 0,
                    model_stats: std::collections::HashMap::new(),
                    ready: true,
                    loaded_models: vec![],
                },
            );
        }

        // Now simulate gossip from that peer with loopback address
        let gossip_peer = PeerState {
            node_id: "remote-peer".into(),
            address: "http://127.0.0.1:8080".into(), // Loopback - should be ignored
            address_from_source: false,
            version: "0.1.0".into(),
            llama_cpp_version: "unknown".into(),
            last_seen: 2000,
            started_at_unix: 0,
            supported_models: vec!["model:default".into(), "new-model:default".into()],
            active_instances: 1,
            max_instances: 8,
            current_requests: 5,
            available_vram: 500,
            available_sysmem: 500,
            llamesh_vram_mb: 0,
            llamesh_sysmem_mb: 0,
            external_vram_mb: 0,
            device_vram_used_mb: 0,
            device_vram_total_mb: 0,
            gpu_telemetry_available: false,
            max_vram: 16000,
            max_sysmem: 64000,
            total_queue_length: 2,
            model_stats: std::collections::HashMap::new(),
            ready: true,
            loaded_models: vec![],
        };

        let msg = GossipMessage {
            origin: gossip_peer,
            known_peers: vec![],
        };

        process_gossip_message(&state, msg, None).await;

        let peers = state.peers.read().await;
        let peer = peers.get("remote-peer").unwrap();

        // Address should be preserved (not overwritten with loopback)
        assert_eq!(peer.address, "http://node-b:8080");

        // But other fields should be updated
        assert_eq!(peer.supported_models.len(), 2);
        assert_eq!(peer.active_instances, 1);
        assert_eq!(peer.current_requests, 5);
    }

    #[test]
    fn test_extract_host_from_url_ipv4() {
        assert_eq!(
            extract_host_from_url("http://192.168.1.1:8080"),
            "192.168.1.1"
        );
        assert_eq!(extract_host_from_url("https://10.0.0.1:443"), "10.0.0.1");
        assert_eq!(extract_host_from_url("http://127.0.0.1"), "127.0.0.1");
    }

    #[test]
    fn test_extract_host_from_url_ipv6() {
        assert_eq!(extract_host_from_url("http://[::1]:8080"), "::1");
        assert_eq!(
            extract_host_from_url("http://[2001:db8::1]:8080"),
            "2001:db8::1"
        );
        assert_eq!(extract_host_from_url("http://[fe80::1]"), "fe80::1");
    }

    #[test]
    fn test_extract_host_from_url_hostname() {
        assert_eq!(extract_host_from_url("http://localhost:8080"), "localhost");
        assert_eq!(
            extract_host_from_url("https://node-a.example.com:443"),
            "node-a.example.com"
        );
        assert_eq!(extract_host_from_url("http://node-b:8080"), "node-b");
    }

    #[test]
    fn test_extract_host_from_url_no_scheme() {
        assert_eq!(extract_host_from_url("192.168.1.1:8080"), "192.168.1.1");
        assert_eq!(extract_host_from_url("localhost:8080"), "localhost");
    }

    async fn address_test_state(seeds: &[&str]) -> NodeState {
        let mut config = minimal_node_config();
        config.cluster.noise.enabled = false;
        config.cluster.peers = seeds.iter().map(|seed| seed.to_string()).collect();
        let build_manager = BuildManager::new(config.llama_cpp.clone());
        NodeState::new(config, Cookbook { models: vec![] }, build_manager)
            .await
            .unwrap()
    }

    async fn receive_address(
        state: &NodeState,
        advertised: &str,
        source: Option<&str>,
    ) -> PeerState {
        let mut origin = peer_state_with_version("peer", env!("CARGO_PKG_VERSION"));
        origin.address = advertised.into();
        origin.current_requests = 7;
        process_gossip_message(
            state,
            GossipMessage {
                origin,
                known_peers: vec![],
            },
            source.map(|source| source.parse().unwrap()),
        )
        .await;
        let peer = state.peers.read().await.get("peer").unwrap().clone();
        assert_eq!(peer.current_requests, 7);
        assert!(peer.last_seen > 1000);
        peer
    }

    #[tokio::test]
    async fn inferred_addresses_follow_direct_gossip_ip_port_and_scheme_changes() {
        let state = address_test_state(&[]).await;
        for (advertised, source, expected) in [
            (
                "http://127.0.0.1:8080",
                "192.0.2.1:50000",
                "http://192.0.2.1:8080",
            ),
            (
                "http://127.0.0.1:8080",
                "192.0.2.2:50001",
                "http://192.0.2.2:8080",
            ),
            (
                "http://127.0.0.1:9000",
                "192.0.2.2:50002",
                "http://192.0.2.2:9000",
            ),
            (
                "http://[::1]:9000",
                "[2001:db8::1]:50003",
                "http://[2001:db8::1]:9000",
            ),
            (
                "http://[::1]:9000",
                "[2001:db8::2]:50004",
                "http://[2001:db8::2]:9000",
            ),
            (
                "https://[::1]:8443",
                "[2001:db8::2]:50005",
                "https://[2001:db8::2]:8443",
            ),
        ] {
            let peer = receive_address(&state, advertised, Some(source)).await;
            assert_eq!(peer.address, expected);
            assert!(peer.address_from_source);
        }
    }

    #[tokio::test]
    async fn missing_or_unusable_source_preserves_refreshable_address() {
        let state = address_test_state(&[]).await;
        receive_address(&state, "http://127.0.0.1:8080", Some("192.0.2.1:50000")).await;
        for source in [
            None,
            Some("127.0.0.1:50001"),
            Some("[::1]:50001"),
            Some("[::ffff:127.0.0.1]:50001"),
            Some("0.0.0.0:50001"),
            Some("[::]:50001"),
            Some("[::ffff:0.0.0.0]:50001"),
            Some("224.0.0.1:50001"),
            Some("255.255.255.255:50001"),
            Some("[::ffff:224.0.0.1]:50001"),
            Some("[::ffff:255.255.255.255]:50001"),
            Some("[ff02::1]:50001"),
            Some("[fe80::1%2]:50001"),
        ] {
            let peer = receive_address(&state, "http://127.0.0.1:9000", source).await;
            assert_eq!(peer.address, "http://192.0.2.1:8080");
            assert!(peer.address_from_source);
        }
        for advertised in ["http://127.0.0.1:invalid", "http://127.0.0.1:0"] {
            let peer = receive_address(&state, advertised, Some("192.0.2.2:50002")).await;
            assert_eq!(peer.address, "http://192.0.2.1:8080");
            assert!(peer.address_from_source);
        }
        let peer = receive_address(&state, "http://127.0.0.1:9000", Some("192.0.2.2:50003")).await;
        assert_eq!(peer.address, "http://192.0.2.2:9000");
        assert!(peer.address_from_source);
    }

    #[tokio::test]
    async fn advertised_hostname_and_public_ip_urls_take_and_keep_precedence() {
        for advertised in [
            "https://gateway.example:9443/mesh",
            "https://192.0.2.1:9443",
        ] {
            let state = address_test_state(&["http://peer:8888"]).await;
            // A direct public advertisement also overrides a selected seed.
            receive_address(&state, "http://127.0.0.1:8080", Some("192.0.2.1:50000")).await;
            let peer = receive_address(&state, advertised, Some("192.0.2.2:50001")).await;
            assert_eq!(peer.address, advertised);
            assert!(!peer.address_from_source);
            let peer =
                receive_address(&state, "http://127.0.0.1:9000", Some("192.0.2.3:50002")).await;
            assert_eq!(peer.address, advertised);
            assert!(!peer.address_from_source);
        }
    }

    #[tokio::test]
    async fn current_public_advertisement_promotes_an_inferred_address() {
        let state = address_test_state(&[]).await;
        receive_address(&state, "http://127.0.0.1:8080", Some("192.0.2.1:50000")).await;
        let peer = receive_address(&state, "http://192.0.2.1:8080", Some("192.0.2.2:50001")).await;
        assert!(!peer.address_from_source);
        let peer = receive_address(&state, "http://127.0.0.1:9000", Some("192.0.2.3:50002")).await;
        assert_eq!(peer.address, "http://192.0.2.1:8080");
        assert!(!peer.address_from_source);
    }

    #[tokio::test]
    async fn configured_seeds_protect_hostname_and_matching_inferred_routes() {
        let state = address_test_state(&["https://peer:9443"]).await;
        for source in ["192.0.2.1:50000", "192.0.2.2:50001"] {
            let peer = receive_address(&state, "http://127.0.0.1:8080", Some(source)).await;
            assert_eq!(peer.address, "https://peer:9443");
            assert!(!peer.address_from_source);
        }
        // This seed's host is not the node ID. Once the derived URL matches it,
        // preserve the explicitly configured route rather than following ingress.
        let state = address_test_state(&["192.0.2.1:80/"]).await;
        receive_address(&state, "http://127.0.0.1:80", Some("192.0.2.1:50000")).await;
        let peer = receive_address(&state, "http://127.0.0.1:9000", Some("192.0.2.2:50001")).await;
        assert_eq!(peer.address, "http://192.0.2.1:80");
        assert!(!peer.address_from_source);
    }

    #[tokio::test]
    async fn transitive_public_routes_are_not_mistaken_for_local_inference() {
        for address in ["https://gateway.example:9443", "https://192.0.2.1:9443"] {
            let state = address_test_state(&[]).await;
            process_gossip_message(
                &state,
                GossipMessage {
                    origin: peer_state_with_version("relay", env!("CARGO_PKG_VERSION")),
                    known_peers: vec![PeerInfo {
                        node_id: "peer".into(),
                        address: address.into(),
                    }],
                },
                Some("192.0.2.10:50000".parse().unwrap()),
            )
            .await;
            let peer =
                receive_address(&state, "http://127.0.0.1:8080", Some("192.0.2.2:50001")).await;
            assert_eq!(peer.address, address);
            assert!(!peer.address_from_source);
        }
    }

    #[test]
    fn seed_url_comparison_only_normalizes_equivalent_urls() {
        for (left, right) in [
            ("peer:80", "http://peer/"),
            ("HTTPS://PEER:443/mesh", "https://peer/mesh"),
            ("[2001:db8::1]:8080", "http://[2001:db8::1]:8080/"),
        ] {
            assert!(same_peer_url(left, right));
        }
        for right in [
            "https://peer:80/mesh",
            "http://peer:8080/mesh",
            "http://peer/other",
            "http://peer/mesh?token=value",
            "http://other/mesh",
        ] {
            assert!(!same_peer_url("http://peer/mesh", right));
        }
        assert!(!same_peer_url("http://[", "http://["));
    }

    #[test]
    fn source_urls_use_listener_ports_and_standard_scheme_defaults() {
        for (advertised, expected) in [
            ("127.0.0.1:8080", "http://192.0.2.1:8080"),
            ("http://localhost", "http://192.0.2.1:80"),
            ("https://[::1]", "https://192.0.2.1:443"),
        ] {
            assert_eq!(
                source_peer_url(advertised, "192.0.2.1:50000".parse().unwrap()).as_deref(),
                Some(expected)
            );
        }
        assert_eq!(
            source_peer_url(
                "http://[::1]:8080",
                "[::ffff:192.0.2.1]:50000".parse().unwrap()
            )
            .as_deref(),
            Some("http://192.0.2.1:8080")
        );
    }
}
