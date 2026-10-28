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

async fn queue_waiter(
    state: &Arc<NodeState>,
    wait_ms: u64,
) -> tokio::task::JoinHandle<Result<Arc<RwLock<Instance>>, NodeError>> {
    let mut profile = sample_profile();
    profile.max_wait_in_queue_ms = Some(wait_ms);
    profile.max_queue_size = Some(0);
    let state = state.clone();
    tokio::spawn(async move { state.get_instance_for_model("queued", &profile, true).await })
}

async fn assert_queue_drained(state: &NodeState) {
    assert!(state.queues.read().await.is_empty());
    assert!(state.pending_tokens.read().await.is_empty());
    assert_eq!(
        state.metrics.queue_pending_tokens.load(Ordering::Relaxed),
        0
    );
    assert!(state.is_idle().await);
}

async fn shutdown_wakes_local_queue(wait_ms: u64) {
    let state = state().await;
    // A pre-map reservation fills the profile limit without starting a process.
    let _blocker = state
        .spawn_reservations
        .reserve("queued:default".into(), (0, 0), None);
    let mut waiters = Vec::new();
    for count in 1..=3 {
        waiters.push(queue_waiter(&state, wait_ms).await);
        timeout(Duration::from_secs(1), async {
            loop {
                if state
                    .queues
                    .read()
                    .await
                    .get("queued:default")
                    .map_or(0, VecDeque::len)
                    == count
                {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("request did not join the local queue");
    }
    assert!(!state.is_idle().await);
    let capacity = state.capacity_notify.notified();
    let shutdown = state.shutdown_notify.notified();
    tokio::pin!(capacity, shutdown);
    capacity.as_mut().enable();
    shutdown.as_mut().enable();
    state.begin_shutdown_drain().await;
    timeout(Duration::from_secs(1), capacity).await.unwrap();
    timeout(Duration::from_secs(1), shutdown).await.unwrap();
    for waiter in waiters {
        let result = timeout(Duration::from_secs(1), waiter)
            .await
            .expect("shutdown left a local queue waiter asleep")
            .unwrap();
        assert!(matches!(result, Err(NodeError::Draining)), "{result:?}");
    }
    assert_queue_drained(&state).await;
}

#[tokio::test]
async fn shutdown_drain_wakes_infinite_local_queue_waiters() {
    shutdown_wakes_local_queue(0).await;
}

#[tokio::test]
async fn shutdown_drain_wakes_finite_local_queue_waiters() {
    shutdown_wakes_local_queue(60_000).await;
}

async fn shutdown_after_wake_before_enqueue(fairness: bool) {
    for wait_ms in [0, 60_000] {
        let state = state().await;
        let _blocker = state
            .spawn_reservations
            .reserve("queued:default".into(), (0, 0), None);
        // A promoted waiter forces the fairness enqueue site rather than admission.
        let mut prior = if fairness {
            Some(
                state
                    .promote_pending_token("queued:default".into(), 100)
                    .await,
            )
        } else {
            None
        };
        let (entered_tx, entered_rx) = oneshot::channel();
        let (release_tx, release_rx) = oneshot::channel();
        *state.test_hooks.before_enqueue.lock() = Some((entered_tx, release_rx));
        let waiter = queue_waiter(&state, wait_ms).await;
        timeout(Duration::from_secs(1), entered_rx)
            .await
            .unwrap()
            .unwrap();
        assert!(state.queues.read().await.is_empty());
        state.begin_shutdown_drain().await;
        release_tx.send(()).unwrap();
        let result = timeout(Duration::from_secs(1), waiter)
            .await
            .expect("enqueue after shutdown wake must not sleep")
            .unwrap();
        assert!(matches!(result, Err(NodeError::Draining)), "{result:?}");
        if let Some(ref mut token) = prior {
            token.release().await;
        }
        assert_queue_drained(&state).await;
    }
}

#[tokio::test]
async fn shutdown_drain_rejects_admission_enqueue_after_wake() {
    shutdown_after_wake_before_enqueue(false).await;
}

#[tokio::test]
async fn shutdown_drain_rejects_fairness_enqueue_after_wake() {
    shutdown_after_wake_before_enqueue(true).await;
}

#[tokio::test]
async fn shutdown_drain_leaves_no_queue_entry_for_a_request_cancelled_after_enqueuing() {
    // Shutdown finishes while a request is about to queue. Whether or not the
    // request then queues, cancelling it at the first await after the queue
    // push must not leave an entry behind, which would hold shutdown open.
    let state = state().await;
    let _blocker = state
        .spawn_reservations
        .reserve("queued:default".into(), (0, 0), None);
    let (before_entered_tx, before_entered_rx) = oneshot::channel();
    let (before_release_tx, before_release_rx) = oneshot::channel();
    *state.test_hooks.before_enqueue.lock() = Some((before_entered_tx, before_release_rx));
    let (after_entered_tx, after_entered_rx) = oneshot::channel();
    let (_after_release_tx, after_release_rx) = oneshot::channel();
    *state.test_hooks.after_enqueue.lock() = Some((after_entered_tx, after_release_rx));

    let mut waiter = queue_waiter(&state, 0).await;
    timeout(Duration::from_secs(1), before_entered_rx)
        .await
        .unwrap()
        .unwrap();
    state.begin_shutdown_drain().await;
    before_release_tx.send(()).unwrap();

    tokio::select! {
        result = &mut waiter => {
            let result = result.unwrap();
            assert!(matches!(result, Err(NodeError::Draining)), "{result:?}");
        }
        _ = after_entered_rx => {
            // Queued after the shutdown wake; the client goes away here.
            waiter.abort();
            let _ = waiter.await;
        }
    }
    assert_queue_drained(&state).await;
}
