use crate::node_state::NodeState;
use crate::node_state::PeerState;
use crate::security::PeerIdentity;
use axum::{
    extract::{ConnectInfo, State},
    http::StatusCode,
    Json,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Semaphore;
use tracing::{debug, info};

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
                    format!("http://{}", addr)
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
            let circuit_breaker = state.circuit_breaker.clone();

            // Spawn each gossip request to avoid head-of-line blocking in the loop
            tokio::spawn(async move {
                let _permit = permit; // Hold permit until task completes
                match client.post(&url).json(&message).send().await {
                    Ok(_) => {
                        // Record success - resets failure count and closes circuit if half-open
                        circuit_breaker.record_success(&cb_key).await;
                    }
                    Err(e) => {
                        // Record failure - circuit breaker handles escalation and logging
                        circuit_breaker.record_failure(&cb_key).await;
                        debug!("Failed to gossip to {}: {}", url, e);
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

/// Extract the port from a URL (e.g., "http://node-b:8080" -> Some(8080))
fn extract_port_from_url(url: &str) -> Option<u16> {
    let after_scheme = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
        .unwrap_or(url);

    // Handle IPv6 addresses in brackets like [::1]:8080
    if after_scheme.starts_with('[') {
        // Find the closing bracket and then look for port after it
        after_scheme
            .find(']')
            .and_then(|idx| after_scheme[idx + 1..].strip_prefix(':'))
            .and_then(|port_str| port_str.parse().ok())
    } else {
        // Regular host:port format
        after_scheme
            .rsplit(':')
            .next()
            .and_then(|port_str| port_str.parse().ok())
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

pub async fn process_gossip_message(
    state: &NodeState,
    msg: GossipMessage,
    source_addr: Option<std::net::SocketAddr>,
) {
    let mut peers = state.peers.write().await;
    let mut peer_state = msg.origin;

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

        if should_reject {
            tracing::warn!(
                event = "peer_rejected_version_mismatch",
                peer_node_id = %peer_state.node_id,
                peer_version = %peer_state.version,
                local_version = %local_version,
                action = %action,
                "Rejecting peer due to version mismatch"
            );
            return; // Don't process this gossip message
        }

        tracing::warn!(
            "Version mismatch detected: Peer {} is on version {}, but local node is on version {}",
            peer_state.node_id,
            peer_state.version,
            local_version
        );
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

        // 1. Check if we have an existing entry with a non-loopback address
        if let Some(existing) = peers.get(&peer_state.node_id) {
            if !is_loopback_address(&existing.address) {
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

        // 3. Use the source socket address as last resort
        if !resolved {
            if let Some(addr) = source_addr {
                let port = extract_port_from_url(&peer_state.address).unwrap_or(8080);
                let scheme = if peer_state.address.starts_with("https://") {
                    "https"
                } else {
                    "http"
                };
                let derived_address = format!("{}://{}:{}", scheme, addr.ip(), port);
                info!(
                    "Using derived address {} for peer {} from source socket (gossiped address {} is loopback)",
                    derived_address, peer_state.node_id, peer_state.address
                );
                peer_state.address = derived_address;
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
                    version: "unknown".to_string(),
                    last_seen: now_secs,
                    supported_models: vec![],
                    active_instances: 0,
                    max_instances: 0,
                    current_requests: 0,
                    available_vram: 0,
                    available_sysmem: 0,
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
            version: "0.1.0".into(),
            last_seen: 1000,
            supported_models: vec![],
            active_instances: 0,
            max_instances: 8,
            current_requests: 0,
            available_vram: 1,
            available_sysmem: 1,
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

        // Should have origin peer
        assert!(peers.contains_key("node-origin"));

        // Should have discovered peer
        assert!(peers.contains_key("node-new"));
        let new_peer = peers.get("node-new").unwrap();
        assert_eq!(new_peer.address, "http://node-new");
        assert_eq!(new_peer.version, "unknown");
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
                    version: "0.1.0".into(),
                    last_seen: 1000,
                    supported_models: vec!["model:default".into()],
                    active_instances: 0,
                    max_instances: 8,
                    current_requests: 0,
                    available_vram: 1000,
                    available_sysmem: 1000,
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
            version: "0.1.0".into(),
            last_seen: 2000,
            supported_models: vec!["model:default".into(), "new-model:default".into()],
            active_instances: 1,
            max_instances: 8,
            current_requests: 5,
            available_vram: 500,
            available_sysmem: 500,
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

    #[test]
    fn test_extract_port_from_url_ipv4() {
        assert_eq!(extract_port_from_url("http://192.168.1.1:8080"), Some(8080));
        assert_eq!(extract_port_from_url("https://10.0.0.1:443"), Some(443));
    }

    #[test]
    fn test_extract_port_from_url_ipv6() {
        assert_eq!(extract_port_from_url("http://[::1]:8080"), Some(8080));
        assert_eq!(
            extract_port_from_url("http://[2001:db8::1]:9000"),
            Some(9000)
        );
    }

    #[test]
    fn test_extract_port_from_url_no_port() {
        assert_eq!(extract_port_from_url("http://localhost"), None);
        assert_eq!(extract_port_from_url("http://[::1]"), None);
    }

    #[test]
    fn test_extract_port_from_url_invalid() {
        assert_eq!(extract_port_from_url("http://localhost:abc"), None);
        assert_eq!(extract_port_from_url("http://host:99999"), None); // Port > u16::MAX
    }
}
