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
    let version = state.cookbook_version.load(Ordering::SeqCst);
    tokio::spawn(async move {
        state
            .get_instance_for_model("queued", &profile, true, version)
            .await
    })
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

async fn wait_for_queue_length(state: &NodeState, count: usize) {
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
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("queue length did not reach the expected value");
}

async fn assert_cancelled_queue_removed(state: &NodeState) {
    timeout(Duration::from_secs(1), async {
        loop {
            if !state.queues.read().await.contains_key("queued:default")
                && state.total_queue_length().await == 0
                && state.is_idle().await
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("cancelled request left a queue entry behind");
}

async fn cancel_queued_waiter(wait_ms: u64, fairness: bool) {
    let state = state().await;
    let _blocker = state
        .spawn_reservations
        .reserve("queued:default".into(), (0, 0), None);
    let mut prior = if fairness {
        Some(
            state
                .promote_pending_token("queued:default".into(), 100)
                .await,
        )
    } else {
        None
    };
    let waiter = queue_waiter(&state, wait_ms).await;
    wait_for_queue_length(&state, 1).await;
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    if let Some(ref mut permit) = prior {
        permit.release().await;
    }
    assert_cancelled_queue_removed(&state).await;
}

#[tokio::test]
async fn cancelled_admission_waiter_with_infinite_wait_is_removed() {
    cancel_queued_waiter(0, false).await;
}

#[tokio::test]
async fn cancelled_admission_waiter_with_finite_wait_is_removed() {
    cancel_queued_waiter(60_000, false).await;
}

#[tokio::test]
async fn cancelled_fairness_waiter_with_infinite_wait_is_removed() {
    cancel_queued_waiter(0, true).await;
}

#[tokio::test]
async fn cancelled_fairness_waiter_with_finite_wait_is_removed() {
    cancel_queued_waiter(60_000, true).await;
}

#[tokio::test]
async fn cancelled_waiter_before_queue_wait_is_removed() {
    let state = state().await;
    let _blocker = state
        .spawn_reservations
        .reserve("queued:default".into(), (0, 0), None);
    let (entered_tx, entered_rx) = oneshot::channel();
    let (_release_tx, release_rx) = oneshot::channel();
    *state.test_hooks.after_enqueue.lock() = Some((entered_tx, release_rx));
    let waiter = queue_waiter(&state, 0).await;
    timeout(Duration::from_secs(1), entered_rx)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(state.total_queue_length().await, 1);
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    assert_cancelled_queue_removed(&state).await;
}

#[tokio::test]
async fn cancelled_waiter_preserves_and_wakes_survivor() {
    let state = state().await;
    let _blocker = state
        .spawn_reservations
        .reserve("queued:default".into(), (0, 0), None);
    let cancelled = queue_waiter(&state, 0).await;
    wait_for_queue_length(&state, 1).await;
    let survivor = queue_waiter(&state, 0).await;
    wait_for_queue_length(&state, 2).await;
    let survivor_token = state.queues.read().await["queued:default"][1].token;
    cancelled.abort();
    assert!(cancelled.await.unwrap_err().is_cancelled());
    wait_for_queue_length(&state, 1).await;
    assert_eq!(
        state.queues.read().await["queued:default"][0].token,
        survivor_token
    );

    // The survivor retries admission after notification, then pauses before requeueing.
    let (entered_tx, entered_rx) = oneshot::channel();
    let (_release_tx, release_rx) = oneshot::channel();
    *state.test_hooks.before_enqueue.lock() = Some((entered_tx, release_rx));
    assert!(state.notify_queue("queued", "default").await);
    timeout(Duration::from_secs(1), entered_rx)
        .await
        .expect("surviving waiter was not woken")
        .unwrap();
    survivor.abort();
    assert!(survivor.await.unwrap_err().is_cancelled());
    assert_queue_drained(&state).await;
}

#[tokio::test]
async fn cancelled_waiter_no_longer_needs_competitor_eviction() {
    let state = state().await;
    let _blocker = state
        .spawn_reservations
        .reserve("queued:default".into(), (0, 0), None);
    let waiter = queue_waiter(&state, 0).await;
    wait_for_queue_length(&state, 1).await;
    state
        .needs_eviction
        .write()
        .await
        .insert("queued:default".into());
    assert!(state.has_queued_competitors_needing_eviction("other").await);
    waiter.abort();
    assert!(waiter.await.unwrap_err().is_cancelled());
    timeout(Duration::from_secs(1), async {
        while state.has_queued_competitors_needing_eviction("other").await {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("cancelled waiter still demands competitor eviction");
    assert_cancelled_queue_removed(&state).await;
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

async fn reload_queued_profile(state: &NodeState, path: &std::path::Path, profiles: &str) {
    tokio::fs::write(
        path,
        format!("models:\n  - name: queued\n    profiles: {profiles}\n"),
    )
    .await
    .unwrap();
    state.reload_cookbook(path).await.unwrap();
}

const QUEUED_PROFILE: &str = "[{id: default, max_instances: 1, model_path: /tmp/model.gguf, llama_server_args: '--ctx-size 1024'}]";
const RELOADED_PROFILE: &str = "[{id: default, max_instances: 1, model_path: /tmp/model.gguf, llama_server_args: '--ctx-size 2048'}]";
const DISABLED_PROFILE: &str = "[{id: default, enabled: false, max_instances: 1, model_path: /tmp/model.gguf, llama_server_args: '--ctx-size 1024'}]";

async fn queued_request_fails_when_profile_stops_resolving(profiles: &str) {
    let (state, dir) = guardrail_tests::spawnable_state().await;
    let path = dir.path().join("cookbook.yaml");
    reload_queued_profile(&state, &path, QUEUED_PROFILE).await;
    let blocker = state
        .spawn_reservations
        .reserve("queued:default".into(), (0, 0), None);
    let waiter = queue_waiter(&state, 0).await;
    wait_for_queue_length(&state, 1).await;

    reload_queued_profile(&state, &path, profiles).await;
    let result = timeout(Duration::from_secs(1), waiter).await;
    drop(blocker);
    let error = result
        .expect("removed profile must fail promptly while capacity stays blocked")
        .unwrap()
        .unwrap_err();
    assert!(
        matches!(error, NodeError::ModelNotFound(ref name) if name == "queued:default"),
        "{error:?}"
    );
    assert_queue_drained(&state).await;
}

#[tokio::test]
async fn queued_request_across_cookbook_reload_returns_model_not_found() {
    queued_request_fails_when_profile_stops_resolving("[]").await;
}

#[tokio::test]
async fn queued_request_across_cookbook_reload_of_disabled_profile_returns_model_not_found() {
    queued_request_fails_when_profile_stops_resolving(DISABLED_PROFILE).await;
}

#[tokio::test]
async fn queued_request_across_cookbook_reload_spawns_new_args() {
    let (state, dir) = guardrail_tests::spawnable_state().await;
    let path = dir.path().join("cookbook.yaml");
    reload_queued_profile(&state, &path, QUEUED_PROFILE).await;
    let version = state.cookbook_version.load(Ordering::SeqCst);
    let (_, mut profile) = state.resolve_model("queued:default").await.unwrap();
    // The request only queues so the reload can land. 0 waits forever, so a
    // slow reload cannot surface as QueueTimeout before the hash assert.
    profile.max_wait_in_queue_ms = Some(0);
    let blocker = state
        .spawn_reservations
        .reserve("queued:default".into(), (0, 0), None);
    // The first enqueue is the reservation block. Release that pause, then
    // arm the hook again so the post-reload requeue is the one we observe.
    // Reloading before the blocker drops forces a re-resolve while admission
    // is still impossible, so the spawn cannot start on the old profile.
    let (first_enqueued, first_release) =
        guardrail_tests::install_pause(&state.test_hooks.after_enqueue);
    let caller = state.clone();
    let waiter = tokio::spawn(async move {
        caller
            .get_instance_for_model("queued", &profile, true, version)
            .await
    });
    timeout(Duration::from_secs(1), first_enqueued)
        .await
        .unwrap()
        .unwrap();
    first_release.send(()).unwrap();
    wait_for_queue_length(&state, 1).await;

    let (reenqueued, re_release) = guardrail_tests::install_pause(&state.test_hooks.after_enqueue);
    reload_queued_profile(&state, &path, RELOADED_PROFILE).await;
    timeout(Duration::from_secs(1), reenqueued)
        .await
        .unwrap()
        .unwrap();
    // Free the slot before the waiter's admission re-check runs, so that
    // check, rather than an unrelated wake, starts the spawn.
    let (entered, release) = guardrail_tests::install_pause(&state.test_hooks.after_port);
    drop(blocker);
    re_release.send(()).unwrap();
    timeout(Duration::from_secs(1), entered)
        .await
        .unwrap()
        .unwrap();
    release.send(()).unwrap();

    let instance = timeout(Duration::from_secs(1), async {
        loop {
            if let Some(instance) = state.instances.read().await.values().next().cloned() {
                break instance;
            }
            tokio::task::yield_now().await;
        }
    })
    .await
    .expect("queued request did not spawn");
    let inst = instance.read().await;
    let actual_hash = inst.args_hash.clone();
    *inst.status.lock() = InstanceStatus::Ready;
    inst.ready_signal.notify_waiters();
    drop(inst);
    let returned = timeout(Duration::from_secs(1), waiter)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    assert!(Arc::ptr_eq(&instance, &returned));
    instance.read().await.stop().await.unwrap();
    state.instances.write().await.clear();
    assert_queue_drained(&state).await;
    let (_, current) = state.resolve_model("queued:default").await.unwrap();
    assert_eq!(actual_hash, args_hash_for_profile(&current));
}

async fn spawn_across_cookbook_reload(changed: bool) {
    let (state, dir) = guardrail_tests::spawnable_state().await;
    let path = dir.path().join("cookbook.yaml");
    reload_queued_profile(&state, &path, QUEUED_PROFILE).await;
    let (_, profile) = state.resolve_model("queued:default").await.unwrap();
    let (entered, release) = guardrail_tests::install_pause(&state.test_hooks.after_port);
    let caller = state.clone();
    let spawn = tokio::spawn(async move {
        caller
            .try_get_or_spawn("queued", &profile, true, "test", false)
            .await
    });
    timeout(Duration::from_secs(1), entered)
        .await
        .unwrap()
        .unwrap();
    assert!(state.instances.read().await.is_empty());
    // Captured on this thread. The changed-args test runs on the current-thread
    // runtime so the spawn task's info event is visible here.
    let capture = changed.then(|| crate::test_support::capture_logs(tracing::Level::INFO));
    if changed {
        // Both reloads finish while after_port is held, before the pre-lock
        // version check. The first keeps the args and the second changes
        // them, so the inserted instance must be draining. This pause does
        // not put a reload between that pre-lock snapshot and the write
        // lock; the atomic re-read under the lock covers that window.
        reload_queued_profile(&state, &path, QUEUED_PROFILE).await;
        reload_queued_profile(&state, &path, RELOADED_PROFILE).await;
    } else {
        let cookbook = format!(
            "models:\n  - name: queued\n    profiles: {QUEUED_PROFILE}\n  - name: unrelated\n    profiles: {RELOADED_PROFILE}\n"
        );
        tokio::fs::write(&path, cookbook).await.unwrap();
        state.reload_cookbook(&path).await.unwrap();
    }
    release.send(()).unwrap();
    let instance = timeout(Duration::from_secs(1), spawn)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let inst = instance.read().await;
    let draining = inst.draining.load(Ordering::Relaxed);
    let in_flight = inst.in_flight_requests;
    inst.stop().await.unwrap();
    assert_eq!(in_flight, 1, "spawning request retains its slot");
    assert_eq!(draining, changed);
    if let Some(capture) = capture {
        let logged = capture.contents();
        assert_eq!(
            logged
                .matches("instance_spawned_across_cookbook_reload")
                .count(),
            1,
            "{logged}"
        );
        assert!(logged.contains("args_changed"), "{logged}");
    }
}

#[tokio::test(flavor = "current_thread")]
async fn spawn_across_cookbook_reload_with_changed_args_drains() {
    spawn_across_cookbook_reload(true).await;
}

#[tokio::test]
async fn spawn_across_cookbook_reload_with_unrelated_change_does_not_drain() {
    spawn_across_cookbook_reload(false).await;
}

#[tokio::test]
async fn every_request_queued_for_a_removed_profile_returns_model_not_found() {
    let (state, dir) = guardrail_tests::spawnable_state().await;
    let path = dir.path().join("cookbook.yaml");
    reload_queued_profile(&state, &path, QUEUED_PROFILE).await;
    let blocker = state
        .spawn_reservations
        .reserve("queued:default".into(), (0, 0), None);
    let first = queue_waiter(&state, 0).await;
    wait_for_queue_length(&state, 1).await;
    let second = queue_waiter(&state, 0).await;
    wait_for_queue_length(&state, 2).await;

    // The reload wakes one waiter for the queue; the second must still fail
    // promptly rather than wait for an unrelated capacity event.
    reload_queued_profile(&state, &path, "[]").await;
    for waiter in [first, second] {
        let error = timeout(Duration::from_secs(1), waiter)
            .await
            .expect("every waiter for a removed profile must fail promptly")
            .unwrap()
            .unwrap_err();
        assert!(matches!(error, NodeError::ModelNotFound(_)), "{error:?}");
    }
    drop(blocker);
    assert_queue_drained(&state).await;
}

#[tokio::test]
async fn request_resolved_before_a_reload_uses_the_reloaded_cookbook() {
    let (state, dir) = guardrail_tests::spawnable_state().await;
    let path = dir.path().join("cookbook.yaml");
    reload_queued_profile(&state, &path, QUEUED_PROFILE).await;
    // As the router does: sample the version, then resolve.
    let version = state.cookbook_version.load(Ordering::SeqCst);
    let (_, profile) = state.resolve_model("queued:default").await.unwrap();
    // The reload lands after resolution but before the request reaches
    // instance acquisition.
    reload_queued_profile(&state, &path, "[]").await;
    let error = timeout(
        Duration::from_secs(1),
        state.get_instance_for_model("queued", &profile, true, version),
    )
    .await
    .expect("a profile removed after resolution must fail promptly")
    .unwrap_err();
    assert!(matches!(error, NodeError::ModelNotFound(_)), "{error:?}");
    assert!(state.instances.read().await.is_empty());
    assert_queue_drained(&state).await;
}

#[tokio::test(flavor = "current_thread")]
async fn reload_between_the_spawn_check_and_insertion_drains_the_instance() {
    let (state, dir) = guardrail_tests::spawnable_state().await;
    let path = dir.path().join("cookbook.yaml");
    reload_queued_profile(&state, &path, QUEUED_PROFILE).await;
    let (_, profile) = state.resolve_model("queued:default").await.unwrap();
    let (entered, release) = guardrail_tests::install_pause(&state.test_hooks.before_insert);
    let caller = state.clone();
    let spawn = tokio::spawn(async move {
        caller
            .try_get_or_spawn("queued", &profile, true, "test", false)
            .await
    });
    timeout(Duration::from_secs(1), entered)
        .await
        .unwrap()
        .unwrap();
    // The spawn has already compared its profile with the cookbook; only the
    // version re-read under the instance map lock can see this reload.
    let capture = crate::test_support::capture_logs(tracing::Level::INFO);
    reload_queued_profile(&state, &path, RELOADED_PROFILE).await;
    release.send(()).unwrap();
    let instance = timeout(Duration::from_secs(1), spawn)
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    let inst = instance.read().await;
    let draining = inst.draining.load(Ordering::Relaxed);
    inst.stop().await.unwrap();
    assert!(
        draining,
        "a reload before insertion must drain the instance"
    );
    let logged = capture.contents();
    assert!(logged.contains("cookbook_changed"), "{logged}");
}
