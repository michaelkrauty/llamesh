use super::tests::{gpu_snapshot, minimal_node_config, sample_profile};
use super::*;
use tokio::time::timeout;

async fn state() -> Arc<NodeState> {
    let mut config = minimal_node_config();
    config.cluster.enabled = false;
    config.metrics_path = format!("/tmp/llamesh-drain-{}.json", ulid::Ulid::new());
    config.llama_cpp.binary_path = "/nonexistent/llamesh-drain-server".into();
    let build = BuildManager::new(config.llama_cpp.clone());
    let state = Arc::new(
        NodeState::new(config, Cookbook { models: vec![] }, build)
            .await
            .unwrap(),
    );
    state
        .memory_sampler
        .set_device_vram_override(Some(gpu_snapshot(0, 1024)));
    state
}

async fn add_draining_instance(
    state: &NodeState,
    id: &str,
    scheduler: bool,
) -> Arc<RwLock<Instance>> {
    let mut instance = Instance::new(
        id.into(),
        id.into(),
        "default".into(),
        "127.0.0.1".into(),
        0,
        id.into(),
        false,
    );
    instance.in_flight_requests = 1;
    instance.draining.store(true, Ordering::Relaxed);
    instance
        .draining_for_competitor
        .store(scheduler, Ordering::Relaxed);
    let instance = Arc::new(RwLock::new(instance));
    state
        .instances
        .write()
        .await
        .insert(id.into(), instance.clone());
    instance
}

#[tokio::test]
async fn abandoned_admission_cancels_scheduler_drains_but_preserves_forced_drains() {
    let state = state().await;
    let scheduler = add_draining_instance(&state, "scheduler", true).await;
    let other_scheduler = add_draining_instance(&state, "other-scheduler", true).await;
    let forced = add_draining_instance(&state, "forced", false).await;
    for instance in [&scheduler, &other_scheduler, &forced] {
        tests::attach_live_test_child(&*instance.read().await);
    }
    let (entered_tx, entered_rx) = tokio::sync::oneshot::channel();
    let (_release_tx, release_rx) = tokio::sync::oneshot::channel();
    *state.test_hooks.after_reserve.lock() = Some((entered_tx, release_rx));
    let spawn_state = state.clone();
    let spawning = tokio::spawn(async move {
        spawn_state
            .try_get_or_spawn("competitor", &sample_profile(), true, "test", false)
            .await
    });
    timeout(Duration::from_secs(1), entered_rx)
        .await
        .unwrap()
        .unwrap();

    // No request-completion or maintenance event follows this cancellation.
    // The admission guard's abandonment callback must resume the incumbents.
    let notified = state.capacity_notify.notified();
    tokio::pin!(notified);
    notified.as_mut().enable();
    spawning.abort();
    assert!(spawning.await.unwrap_err().is_cancelled());
    timeout(Duration::from_secs(1), notified).await.unwrap();
    for instance in [&scheduler, &other_scheduler, &forced] {
        instance.read().await.stop().await.unwrap();
    }
    assert!(!scheduler.read().await.draining.load(Ordering::Relaxed));
    assert!(!other_scheduler
        .read()
        .await
        .draining
        .load(Ordering::Relaxed));
    assert!(forced.read().await.draining.load(Ordering::Relaxed));
}

#[tokio::test]
async fn scheduler_drain_remains_while_competitor_is_queued() {
    let state = state().await;
    let incumbent = add_draining_instance(&state, "incumbent", true).await;
    let key = "competitor:default".to_string();
    let (tx, _rx) = tokio::sync::oneshot::channel();
    state.needs_eviction.write().await.insert(key.clone());
    state
        .queues
        .write()
        .await
        .insert(key.clone(), VecDeque::from([QueueEntry { token: 1, tx }]));
    state.maybe_cancel_drains().await;
    assert!(incumbent.read().await.draining.load(Ordering::Relaxed));
    state.queues.write().await.remove(&key);
    state.maybe_cancel_drains().await;
    assert!(!incumbent.read().await.draining.load(Ordering::Relaxed));
}

#[tokio::test(start_paused = true)]
async fn forced_drains_promote_scheduler_drains_and_cannot_be_cancelled() {
    for reason in ["cookbook", "binary", "oom"] {
        let state = state().await;
        let incumbent = add_draining_instance(&state, "incumbent", true).await;
        // Readiness holds a read guard while loading. Mandatory drains must
        // still be marked promptly, without waiting for startup to finish.
        let loading = incumbent.read().await;
        match reason {
            "cookbook" => {
                timeout(
                    Duration::from_millis(10),
                    state.reconcile_instances_with_cookbook(),
                )
                .await
                .unwrap();
            }
            "binary" => timeout(
                Duration::from_millis(10),
                state.drain_instances_for_binary_update(),
            )
            .await
            .unwrap(),
            "oom" => {
                // Pause recovery while its busy incumbent drains.
                assert!(timeout(
                    Duration::from_millis(10),
                    state.evict_all_instances_gracefully()
                )
                .await
                .is_err());
            }
            _ => unreachable!(),
        }
        assert!(
            !loading.draining_for_competitor.load(Ordering::Relaxed),
            "{reason} did not promote during loading"
        );
        drop(loading);
        state.maybe_cancel_drains().await;
        let instance = incumbent.read().await;
        assert!(
            instance.draining.load(Ordering::Relaxed),
            "{reason} drain cancelled"
        );
        assert!(
            !instance.draining_for_competitor.load(Ordering::Relaxed),
            "{reason} did not promote the scheduler drain"
        );
    }
}
