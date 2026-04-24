use crate::node_state::NodeState;
use axum::{http::StatusCode, response::IntoResponse, Json};
use serde_json::json;
use std::sync::atomic::Ordering;
use std::sync::Arc;

pub async fn healthz(state: axum::extract::State<Arc<NodeState>>) -> impl IntoResponse {
    // Check if process is up (implicit)
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
        }
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
