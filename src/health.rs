use crate::node_state::NodeState;
use axum::{http::StatusCode, response::IntoResponse, Json};
use serde_json::json;
use std::sync::atomic::Ordering;
use std::sync::Arc;

/// Whether the node is forwarding-only: a cluster member with an empty
/// cookbook. It routes every request to its peers and never starts an
/// instance, so it needs neither local models nor a `llama-server` binary.
async fn is_forwarding_only(state: &NodeState) -> bool {
    state.config.cluster.enabled && state.cookbook.read().await.models.is_empty()
}

pub async fn healthz(state: axum::extract::State<Arc<NodeState>>) -> impl IntoResponse {
    // Check if process is up (implicit)
    if is_forwarding_only(&state).await {
        return (StatusCode::OK, Json(json!({"status": "ok"})));
    }

    // Check if cookbook has models
    let cookbook = state.cookbook.read().await;
    if cookbook.models.is_empty() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status": "error", "message": "No models configured"})),
        );
    }
    drop(cookbook);

    if !state.build_manager.is_binary_available_cached() {
        // Optional strict check: binary must exist for health? Or just for readiness?
        // Usually healthz is "am I broken?", readyz is "can I serve traffic?"
        // If binary is missing, we probably can't serve, but are we "broken"?
        // Yes, if build failed we are broken.
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status": "error", "message": "Binary missing"})),
        );
    }

    (StatusCode::OK, Json(json!({"status": "ok"})))
}

pub async fn readyz(state: axum::extract::State<Arc<NodeState>>) -> impl IntoResponse {
    if state.draining.load(Ordering::Relaxed) {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status": "draining", "message": "Node is draining"})),
        );
    }

    // A forwarding-only node can serve a request only by forwarding it, so it
    // is ready while some peer could take one.
    if is_forwarding_only(&state).await {
        if !state.has_forwarding_target().await {
            return (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(json!({"status": "not_ready", "message": "No peer available"})),
            );
        }
        return (
            StatusCode::OK,
            Json(json!({"status": "ready", "can_spawn": false})),
        );
    }

    let binary_available = state.build_manager.is_binary_available_cached();
    if !binary_available {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status": "not_ready", "message": "Binary missing"})),
        );
    }

    // Check if models are configured (consistent with healthz)
    let cookbook = state.cookbook.read().await;
    if cookbook.models.is_empty() {
        return (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(json!({"status": "not_ready", "message": "No models configured"})),
        );
    }
    drop(cookbook);

    // Report coarse actual resource availability. Per-profile admission still
    // performs the definitive check with model-specific memory estimates.
    let (current_vram, current_sysmem) = state.calculate_current_load().await;
    let can_spawn = state.build_manager.can_serve()
        && current_vram < state.config.max_vram_mb
        && current_sysmem < state.config.max_sysmem_mb;

    (
        StatusCode::OK,
        Json(json!({"status": "ready", "can_spawn": can_spawn})),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build_manager::BuildManager;
    use crate::config::{
        ClusterConfig, Cookbook, HttpConfig, LlamaCppConfig, ModelDefaults, NodeConfig,
    };
    use crate::node_state::PeerState;
    use axum::extract::State;

    fn minimal_node_config() -> NodeConfig {
        NodeConfig {
            node_id: "node-test".to_string(),
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
            },
            cluster: ClusterConfig {
                enabled: false,
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
            upstream_read_timeout_ms: 600_000,
            wedge_detector: crate::config::WedgeDetectorConfig::default(),
        }
    }

    /// A cluster member that starts no Noise context or mDNS discovery, so a
    /// test creates no key files and sends nothing on the network.
    fn cluster_node_config() -> NodeConfig {
        let mut config = minimal_node_config();
        config.cluster.enabled = true;
        config.cluster.noise.enabled = false;
        config.cluster.discovery.mdns = false;
        config
    }

    async fn node_state(config: NodeConfig, cookbook: Cookbook) -> Arc<NodeState> {
        let build_manager = BuildManager::new(config.llama_cpp.clone());
        Arc::new(
            crate::node_state::NodeState::new(config, cookbook, build_manager)
                .await
                .unwrap(),
        )
    }

    fn one_model_cookbook() -> Cookbook {
        serde_yaml::from_str(
            r#"
models:
  - name: "local-model"
    profiles:
      - id: "default"
        model_path: "/nonexistent/model.gguf"
        llama_server_args: ""
"#,
        )
        .unwrap()
    }

    async fn body_json(response: axum::response::Response) -> serde_json::Value {
        let bytes = axum::body::to_bytes(response.into_body(), usize::MAX)
            .await
            .unwrap();
        serde_json::from_slice(&bytes).unwrap()
    }

    /// A peer as gossip reports it, last seen now.
    fn peer(node_id: &str, ready: bool, supported_models: &[&str]) -> PeerState {
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        serde_json::from_value(json!({
            "node_id": node_id,
            "address": format!("http://{node_id}:8080"),
            "version": "0.0.0",
            "last_seen": now,
            "supported_models": supported_models,
            "active_instances": 0,
            "max_instances": 1,
            "current_requests": 0,
            "available_vram": 0,
            "available_sysmem": 0,
            "total_queue_length": 0,
            "ready": ready
        }))
        .unwrap()
    }

    async fn add_peer(state: &NodeState, peer: PeerState) {
        state.peers.write().await.insert(peer.node_id.clone(), peer);
    }

    #[tokio::test]
    async fn forwarding_only_node_is_healthy_and_ready_without_a_binary() {
        // An empty cookbook on a cluster member routes everything to peers;
        // the llama-server binary (missing here) is never needed.
        let state = node_state(cluster_node_config(), Cookbook { models: vec![] }).await;

        let response = healthz(State(state.clone())).await.into_response();
        assert_eq!(response.status(), StatusCode::OK);

        // It is ready only once a peer could take a forwarded request: not a
        // peer that is not ready, or one that advertises no models.
        let response = readyz(State(state.clone())).await.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body_json(response).await["message"], "No peer available");
        add_peer(&state, peer("loading", false, &["remote-model"])).await;
        add_peer(&state, peer("gateway", true, &[])).await;
        let response = readyz(State(state.clone())).await.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);

        add_peer(&state, peer("serving", true, &["remote-model"])).await;
        let response = readyz(State(state.clone())).await.into_response();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            body_json(response).await,
            json!({"status": "ready", "can_spawn": false})
        );

        // A stale peer no longer counts.
        state
            .peers
            .write()
            .await
            .get_mut("serving")
            .unwrap()
            .last_seen = 0;
        let response = readyz(State(state.clone())).await.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        state
            .peers
            .write()
            .await
            .get_mut("serving")
            .unwrap()
            .last_seen = peer("serving", true, &[]).last_seen;

        // Draining still takes it out of rotation.
        state
            .draining
            .store(true, std::sync::atomic::Ordering::Relaxed);
        let response = readyz(State(state)).await.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn cluster_node_with_models_still_needs_its_binary() {
        let state = node_state(cluster_node_config(), one_model_cookbook()).await;

        let response = healthz(State(state.clone())).await.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body_json(response).await["message"], "Binary missing");

        let response = readyz(State(state)).await.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        assert_eq!(body_json(response).await["message"], "Binary missing");
    }

    #[tokio::test]
    async fn test_healthz_no_models() {
        let config = minimal_node_config();
        let cookbook = Cookbook { models: vec![] }; // No models
        let build_manager = BuildManager::new(config.llama_cpp.clone());
        let state = Arc::new(
            crate::node_state::NodeState::new(config, cookbook, build_manager)
                .await
                .unwrap(),
        );

        let response = healthz(State(state)).await.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_readyz_binary_missing() {
        let config = minimal_node_config();
        let cookbook = Cookbook { models: vec![] };
        let build_manager = BuildManager::new(config.llama_cpp.clone());
        let state = Arc::new(
            crate::node_state::NodeState::new(config, cookbook, build_manager)
                .await
                .unwrap(),
        );

        let response = readyz(State(state)).await.into_response();
        // Binary missing should return 503
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }

    #[tokio::test]
    async fn test_readyz_draining() {
        let config = minimal_node_config();
        let cookbook = Cookbook { models: vec![] };
        let build_manager = BuildManager::new(config.llama_cpp.clone());
        let state = Arc::new(
            crate::node_state::NodeState::new(config, cookbook, build_manager)
                .await
                .unwrap(),
        );

        // Set draining flag
        state
            .draining
            .store(true, std::sync::atomic::Ordering::Relaxed);

        let response = readyz(State(state)).await.into_response();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
    }
}
