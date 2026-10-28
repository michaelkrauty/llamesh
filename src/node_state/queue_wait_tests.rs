use super::tests::{gpu_snapshot, minimal_node_config};
use super::*;
use crate::errors::AppError;
use axum::{body::Body, extract::State, http::Request, response::IntoResponse};

async fn state(wait_ms: u64, local: bool) -> (Arc<NodeState>, tempfile::TempDir) {
    let dir = tempfile::tempdir().unwrap();
    let mut config = minimal_node_config();
    config.model_defaults.max_wait_in_queue_ms = wait_ms;
    config.metrics_path = dir
        .path()
        .join("metrics.json")
        .to_string_lossy()
        .into_owned();
    config.llama_cpp.binary_path = dir.path().join("server").to_string_lossy().into_owned();
    let cookbook = if local {
        serde_yaml::from_str("models: [{name: queued, profiles: [{id: default, max_instances: 1, max_wait_in_queue_ms: 100, model_path: /tmp/model.gguf, llama_server_args: ''}]}]").unwrap()
    } else {
        Cookbook { models: vec![] }
    };
    let build = BuildManager::new(config.llama_cpp.clone());
    let state = Arc::new(NodeState::new(config, cookbook, build).await.unwrap());
    state
        .memory_sampler
        .set_device_vram_override(Some(gpu_snapshot(0, 1024)));
    (state, dir)
}

async fn unavailable_peer(state: &NodeState, stale: bool) {
    let mut peer = state.get_self_peer_state().await;
    peer.node_id = "peer".into();
    peer.supported_models = vec!["queued".into()];
    peer.ready = false;
    if stale {
        peer.last_seen = peer.last_seen.saturating_sub(16);
    }
    state.peers.write().await.insert(peer.node_id.clone(), peer);
}

async fn request(state: Arc<NodeState>) -> Result<axum::response::Response, AppError> {
    let req = Request::post("/v1/chat/completions")
        .body(Body::from(r#"{"model":"queued","messages":[]}"#))
        .unwrap();
    crate::router::route_request(State(state), None, req)
        .await
        .map(IntoResponse::into_response)
}

fn assert_queue_timeout(error: AppError, state: &NodeState) {
    assert_eq!(error.status, axum::http::StatusCode::SERVICE_UNAVAILABLE);
    assert_eq!(error.error.type_, "queue_timeout");
    assert_eq!(error.error.message, "Queue timeout");
    assert_eq!(error.headers.unwrap()["retry-after"], "5");
    assert_eq!(state.metrics.queue_drops_timeout.load(Ordering::Relaxed), 1);
    assert_eq!(state.metrics.errors_total.load(Ordering::Relaxed), 1);
    assert_eq!(state.metrics.current_requests.load(Ordering::Relaxed), 0);
}

#[tokio::test(start_paused = true)]
async fn unavailable_peer_capacity_wait_times_out() {
    let capture = crate::test_support::capture_logs(tracing::Level::INFO);
    let (state, _dir) = state(100, false).await;
    unavailable_peer(&state, false).await;
    let start = tokio::time::Instant::now();
    let error = tokio::time::timeout(Duration::from_millis(150), request(state.clone()))
        .await
        .expect("router capacity wait exceeded queue timeout")
        .unwrap_err();
    assert!(start.elapsed() >= Duration::from_millis(100));
    assert_queue_timeout(error, &state);
    let logged = capture.contents();
    let drops: Vec<serde_json::Value> = logged
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .filter(|event: &serde_json::Value| event["fields"]["event"] == "queue_drop")
        .collect();
    assert_eq!(drops.len(), 1, "{logged}");
    assert_eq!(drops[0]["fields"]["reason"], "timeout");
}

#[tokio::test(start_paused = true)]
async fn zero_queue_timeout_keeps_unavailable_peer_wait_pending() {
    let (state, _dir) = state(0, false).await;
    unavailable_peer(&state, false).await;
    assert!(
        tokio::time::timeout(Duration::from_secs(1), request(state.clone()))
            .await
            .is_err()
    );
    assert_eq!(state.metrics.queue_drops_timeout.load(Ordering::Relaxed), 0);
    assert_eq!(state.metrics.current_requests.load(Ordering::Relaxed), 0);
}

#[tokio::test(start_paused = true)]
async fn stale_peer_does_not_advertise_model_for_capacity_wait() {
    let (state, _dir) = state(0, false).await;
    unavailable_peer(&state, true).await;
    let start = tokio::time::Instant::now();
    let error = tokio::time::timeout(Duration::from_millis(10), request(state.clone()))
        .await
        .expect("stale peer kept an unknown model waiting")
        .unwrap_err();
    assert_eq!(error.status, axum::http::StatusCode::NOT_FOUND);
    assert_eq!(error.error.type_, "model_not_found");
    assert!(start.elapsed() < Duration::from_millis(10));
    assert_eq!(state.peers.read().await.len(), 1);
    assert_eq!(state.metrics.queue_drops_timeout.load(Ordering::Relaxed), 0);
}

#[tokio::test(start_paused = true)]
async fn router_and_local_queue_share_one_timeout() {
    // The profile's 100 ms overrides the node's 900 ms default.
    let (state, dir) = state(900, true).await;
    let _blocker = state
        .spawn_reservations
        .reserve("queued:default".into(), (0, 0), None);
    let request = request(state.clone());
    tokio::pin!(request);
    assert!(futures::poll!(&mut request).is_pending());
    assert!(state.queues.read().await.is_empty());
    tokio::time::advance(Duration::from_millis(60)).await;
    // Make the local binary available, while admission remains blocked.
    std::fs::write(dir.path().join("server"), "").unwrap();
    state.build_manager.invalidate_binary_cache();
    state.capacity_notify.notify_waiters();
    tokio::select! {
        result = &mut request => panic!("request ended before local queueing: {result:?}"),
        _ = async {
            while state.total_queue_length().await != 1 {
                tokio::task::yield_now().await;
            }
        } => {}
    }
    let error = tokio::time::timeout(Duration::from_millis(60), &mut request)
        .await
        .expect("local queue restarted the timeout after 60 ms in the router")
        .unwrap_err();
    assert_queue_timeout(error, &state);
    assert!(state.queues.read().await.is_empty());
    assert!(state.pending_tokens.read().await.is_empty());
    let hash = state.get_args_hash_for_key("queued:default").await.unwrap();
    assert_eq!(
        state
            .metrics
            .get_hash_metrics(&hash)
            .await
            .errors_total
            .load(Ordering::Relaxed),
        1
    );
}

#[tokio::test(start_paused = true)]
async fn missing_local_binary_capacity_wait_uses_profile_timeout() {
    let (state, _dir) = state(900, true).await;
    let error = tokio::time::timeout(Duration::from_millis(150), request(state.clone()))
        .await
        .expect("local capacity wait ignored profile timeout")
        .unwrap_err();
    assert_queue_timeout(error, &state);
    let hash = state.get_args_hash_for_key("queued:default").await.unwrap();
    assert_eq!(
        state
            .metrics
            .get_hash_metrics(&hash)
            .await
            .errors_total
            .load(Ordering::Relaxed),
        1
    );
}
