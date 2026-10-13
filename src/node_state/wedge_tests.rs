use super::tests::{attach_live_test_child, minimal_node_config};
use super::*;

async fn state_with_cpu_only_wedge_detector() -> Arc<NodeState> {
    let mut config = minimal_node_config();
    config.cluster.enabled = false;
    config.metrics_path = format!("/tmp/llamesh-wedge-{}.json", ulid::Ulid::new());
    config.wedge_detector.enabled = true;
    config.wedge_detector.window_ms = 0;
    config.wedge_detector.cpu_only = true;
    let build = BuildManager::new(config.llama_cpp.clone());
    let state = Arc::new(
        NodeState::new(config, Cookbook { models: vec![] }, build)
            .await
            .unwrap(),
    );
    // No GPU, so under `cpu_only` an idle CPU alone is the wedge signal.
    state
        .memory_sampler
        .set_gpu_activity_override(false, HashMap::new());
    state
}

/// A ready instance holding one request slot, backed by an idle process.
async fn add_idle_instance_holding_slot(state: &NodeState) -> Arc<RwLock<Instance>> {
    let mut instance = Instance::new(
        "idle".into(),
        "test".into(),
        "default".into(),
        "127.0.0.1".into(),
        0,
        "hash".into(),
        false,
    );
    attach_live_test_child(&instance);
    *instance.status.lock() = InstanceStatus::Ready;
    instance.in_flight_requests = 1;
    let instance = Arc::new(RwLock::new(instance));
    state
        .instances
        .write()
        .await
        .insert("idle".into(), instance.clone());
    instance
}

fn wedge_kills(state: &NodeState) -> u64 {
    state
        .metrics
        .wedged_instances_killed_total
        .load(Ordering::Relaxed)
}

#[tokio::test]
async fn wedge_detector_spares_instance_whose_stream_waits_on_its_client() {
    let state = state_with_cpu_only_wedge_detector().await;
    let instance = add_idle_instance_holding_slot(&state).await;
    let awaiting = instance.read().await.streams_awaiting_downstream.clone();
    awaiting.store(1, Ordering::Relaxed);

    // The first tick records a CPU baseline; later ticks see an idle process.
    for _ in 0..3 {
        state.detect_and_kill_wedged_instances().await;
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    assert!(state.instances.read().await.contains_key("idle"));
    assert_eq!(wedge_kills(&state), 0);

    // Once the request is waiting on the instance rather than its client, the
    // idle instance is wedged.
    awaiting.store(0, Ordering::Relaxed);
    state.detect_and_kill_wedged_instances().await;
    assert!(!state.instances.read().await.contains_key("idle"));
    assert_eq!(wedge_kills(&state), 1);
}
