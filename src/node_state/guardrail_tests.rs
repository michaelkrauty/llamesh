use super::*;
use crate::build_manager::BuildManager;
use crate::config::{
    ClusterConfig, Cookbook, HttpConfig, LlamaCppConfig, Model, ModelDefaults, NodeConfig, Profile,
};
use crate::instance::Instance;
use crate::memory_sampler::{GpuDeviceMemory, GpuMemorySnapshot};
use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;
use tokio::sync::oneshot;
use tokio::time::{timeout, Duration};

/// Deterministic scheduler pause points. They are compiled only in unit tests
/// and are consumed once, so an individual test controls exactly one race.
#[derive(Default)]
pub(super) struct TestHooks {
    pub(super) after_reserve: Mutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>,
    pub(super) before_enqueue: Mutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>,
    pub(super) before_retire: Mutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>,
    pub(super) after_capacity_retire: Mutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>,
    pub(super) after_retirement_send: Mutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>,
}

pub(super) async fn pause_at(hook: &Mutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>) {
    let pause = hook.lock().take();
    if let Some((entered, release)) = pause {
        let _ = entered.send(());
        let _ = release.await;
    }
}

fn install_pause(
    hook: &Mutex<Option<(oneshot::Sender<()>, oneshot::Receiver<()>)>>,
) -> (oneshot::Receiver<()>, oneshot::Sender<()>) {
    let (entered_tx, entered_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    *hook.lock() = Some((entered_tx, release_rx));
    (entered_rx, release_tx)
}

fn test_config() -> NodeConfig {
    NodeConfig {
        node_id: format!("guardrail-{}", ulid::Ulid::new()),
        listen_addr: "127.0.0.1:0".into(),
        public_url: None,
        max_vram_mb: 1_000,
        max_sysmem_mb: 1_000,
        max_instances_per_node: 10,
        metrics_path: std::env::temp_dir()
            .join(format!("llamesh-guardrail-{}.json", ulid::Ulid::new()))
            .to_string_lossy()
            .into_owned(),
        default_model: "a:default".into(),
        model_defaults: ModelDefaults {
            max_concurrent_requests_per_instance: 1,
            max_queue_size_per_model: 4,
            max_instances_per_model: 4,
            max_wait_in_queue_ms: 2_000,
            max_request_duration_ms: 300_000,
            min_eviction_tenure_secs: 0,
        },
        llama_cpp_ports: None,
        llama_cpp: LlamaCppConfig {
            repo_url: String::new(),
            repo_path: ".".into(),
            build_path: ".".into(),
            // All admission-path tests deliberately stop at exec failure.
            binary_path: "/nonexistent/llamesh-guardrail-server".into(),
            branch: "master".into(),
            build_args: vec![],
            build_command_args: vec![],
            auto_update_interval_seconds: 0,
            enabled: false,
            keep_builds: 1,
        },
        cluster: ClusterConfig {
            enabled: false,
            peers: vec![],
            gossip_interval_seconds: 5,
            max_concurrent_gossip: 1,
            discovery: Default::default(),
            noise: Default::default(),
            circuit_breaker: Default::default(),
            version_mismatch_action: "warn".into(),
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
        shutdown_grace_period_seconds: 1,
        max_hops: 1,
        logging: None,
        max_total_queue_entries: 0,
        upstream_read_timeout_ms: 0,
        wedge_detector: Default::default(),
    }
}

async fn test_state(config: NodeConfig) -> Arc<NodeState> {
    test_state_with_cookbook(config, Cookbook { models: vec![] }).await
}

async fn test_state_with_cookbook(config: NodeConfig, cookbook: Cookbook) -> Arc<NodeState> {
    let build_manager = BuildManager::new(config.llama_cpp.clone());
    let max_vram_mb = config.max_vram_mb;
    let state = NodeState::new(config, cookbook, build_manager)
        .await
        .unwrap();
    // Never inherit host GPU telemetry: these tests model all device usage.
    state
        .memory_sampler
        .set_device_vram_override(Some(gpu_snapshot(0, max_vram_mb)));
    Arc::new(state)
}

fn profile(id: &str, vram: u64, sysmem: u64) -> Profile {
    Profile {
        id: id.into(),
        description: None,
        enabled: true,
        model_path: Some(format!("/tmp/{id}.gguf")),
        hf_repo: None,
        hf_file: None,
        idle_timeout_seconds: 600,
        max_instances: Some(4),
        llama_server_args: vec![],
        estimated_vram_mb: Some(vram),
        estimated_sysmem_mb: Some(sysmem),
        max_wait_in_queue_ms: Some(2_000),
        max_request_duration_ms: None,
        startup_timeout_seconds: None,
        download_timeout_seconds: None,
        max_queue_size: Some(4),
        min_eviction_tenure_secs: Some(0),
    }
}

fn gpu_snapshot(used_mb: u64, total_mb: u64) -> GpuMemorySnapshot {
    GpuMemorySnapshot {
        used_mb,
        free_mb: total_mb.saturating_sub(used_mb),
        total_mb,
        devices: vec![GpuDeviceMemory {
            index: 0,
            used_mb,
            free_mb: total_mb.saturating_sub(used_mb),
            total_mb,
        }],
    }
}

async fn wait_for_empty_reservations(state: &NodeState) {
    for _ in 0..20 {
        if state.spawn_reservations.snapshot().entries.is_empty() {
            return;
        }
        tokio::task::yield_now().await;
    }
    panic!(
        "spawn reservation leaked: {:?}",
        state.spawn_reservations.snapshot().entries
    );
}

async fn wait_for_hook(entered: oneshot::Receiver<()>, description: &str) {
    timeout(Duration::from_secs(1), entered)
        .await
        .unwrap_or_else(|_| panic!("timed out waiting for {description}"))
        .unwrap_or_else(|_| panic!("{description} sender dropped"));
}

fn attach_live_child(instance: &Instance) -> u32 {
    let child = tokio::process::Command::new("sleep")
        .arg("60")
        .spawn()
        .expect("spawn sleep child");
    let pid = child.id().expect("sleep child pid");
    *instance.child.lock() = Some(child);
    pid
}

#[tokio::test]
async fn premap_reservation_blocks_real_admission_and_cancellation_releases_it() {
    let state = test_state(test_config()).await;
    let first_profile = profile("first", 600, 100);
    let second_profile = profile("second", 600, 100);
    let (entered, release) = install_pause(&state.test_hooks.after_reserve);

    let first_state = state.clone();
    let first = tokio::spawn(async move {
        first_state
            .try_get_or_spawn("first", &first_profile, false, "test", false)
            .await
    });
    wait_for_hook(entered, "first reservation pause").await;

    let err = state
        .try_get_or_spawn("second", &second_profile, false, "test", false)
        .await
        .expect_err("second 600 MiB admission must see first pre-map reservation");
    assert!(matches!(err, NodeError::InsufficientResources));
    assert_eq!(state.spawn_reservations.node_total(), 1);

    first.abort();
    let _ = release.send(());
    let _ = first.await;
    wait_for_empty_reservations(&state).await;

    let err = state
        .try_get_or_spawn("second", &second_profile, false, "test", false)
        .await
        .expect_err("replacement reaches deliberately nonexistent executable");
    assert!(
        !matches!(err, NodeError::InsufficientResources),
        "cancelled reservation must not continue to consume capacity: {err:?}"
    );
}

#[tokio::test]
async fn pre_exec_resample_sees_external_vram_change_after_reservation() {
    let state = test_state(test_config()).await;
    let requested = profile("a", 400, 100);
    state
        .memory_sampler
        .set_device_vram_override(Some(gpu_snapshot(0, 1_000)));
    let (entered, release) = install_pause(&state.test_hooks.after_reserve);

    let spawn_state = state.clone();
    let spawn = tokio::spawn(async move {
        spawn_state
            .try_get_or_spawn("a", &requested, false, "test", false)
            .await
    });
    wait_for_hook(entered, "reservation pause").await;

    // This change occurs after normal admission but before the exec-time check.
    state
        .memory_sampler
        .set_device_vram_override(Some(gpu_snapshot(700, 1_000)));
    release.send(()).unwrap();
    let err = spawn.await.unwrap().unwrap_err();
    assert!(matches!(err, NodeError::InsufficientResources));
    wait_for_empty_reservations(&state).await;

    // A later attempt must sample the fall rather than retaining the rejection.
    state
        .memory_sampler
        .set_device_vram_override(Some(gpu_snapshot(0, 1_000)));
    let err = state
        .try_get_or_spawn("a", &profile("a", 400, 100), false, "test", false)
        .await
        .expect_err("the executable is intentionally absent");
    assert!(!matches!(err, NodeError::InsufficientResources));
}

#[tokio::test]
async fn pre_exec_counts_its_own_loading_lease_exactly_once() {
    let state = test_state(test_config()).await;
    let requested = profile("a", 400, 100);
    state
        .memory_sampler
        .set_device_vram_override(Some(gpu_snapshot(0, 1_000)));
    let (entered, release) = install_pause(&state.test_hooks.after_reserve);

    let spawn_state = state.clone();
    let spawn = tokio::spawn(async move {
        spawn_state
            .try_get_or_spawn("a", &requested, false, "test", false)
            .await
    });
    wait_for_hook(entered, "reservation pause").await;

    // 500 external + 400 own promise fits exactly once (900), but would fail
    // if pre-exec accounting added the candidate estimate a second time.
    state
        .memory_sampler
        .set_device_vram_override(Some(gpu_snapshot(500, 1_000)));
    release.send(()).unwrap();
    let err = spawn.await.unwrap().unwrap_err();
    assert!(
        !matches!(err, NodeError::InsufficientResources),
        "pre-exec accounting must include its own promise exactly once: {err:?}"
    );
    wait_for_empty_reservations(&state).await;
}

#[tokio::test]
async fn loading_commitment_covers_partial_samples_then_releases_at_ready() {
    let state = test_state(test_config()).await;
    let mut guard = state
        .spawn_reservations
        .reserve("a:default".into(), (600, 400), None);
    let memory = guard.memory();
    let id = memory.id().to_string();
    state
        .memory_sampler
        .set_device_vram_override(Some(gpu_snapshot(300, 1_000)));

    let mut instance = Instance::new(
        id.clone(),
        "a".into(),
        "default".into(),
        "127.0.0.1".into(),
        0,
        "loading-hash".into(),
        true,
    );
    instance.memory_reservation = Some(memory.clone());
    let pid = attach_live_child(&instance);
    memory.set_pid(pid);
    state
        .memory_sampler
        .set_process_memory_override(pid, Some(150), Some(50));
    state
        .instances
        .write()
        .await
        .insert(id.clone(), Arc::new(RwLock::new(instance)));
    guard.handoff();
    drop(guard);

    let accounting = {
        let instances = state.instances.read().await;
        state.resource_accounting_for_instances(&instances).await
    };
    assert_eq!(accounting.resources.llamesh_vram_mb, 150);
    assert_eq!(accounting.resources.external_vram_mb, 150);
    assert_eq!(accounting.resources.effective_vram_mb, 750);
    assert_eq!(accounting.resources.effective_sysmem_mb, 400);
    assert_eq!(accounting.instance_memory[&id], (600, 400));

    // Starting instances cannot teach the learned-memory cache from a partial load.
    state.update_peak_memory("a", "default").await;
    assert!(
        state
            .metrics
            .get_learned_memory("loading-hash")
            .await
            .is_none(),
        "partial loading sample must not become a learned peak"
    );

    {
        let instance = state.instances.read().await[&id].clone();
        *instance.read().await.status.lock() = InstanceStatus::Ready;
    }
    memory.mark_ready();
    state.update_peak_memory("a", "default").await;
    assert_eq!(
        state.metrics.get_learned_memory("loading-hash").await,
        Some((150, 50))
    );

    let peer = state.get_self_peer_state().await;
    assert_eq!(peer.available_vram, 700);
    assert_eq!(peer.available_sysmem, 950);

    let removed = state.instances.write().await.remove(&id).unwrap();
    removed.read().await.stop().await.unwrap();
    drop(removed);
    drop(memory);
    wait_for_empty_reservations(&state).await;
}

#[tokio::test]
async fn ready_rss_without_nvml_vram_keeps_configured_vram_estimate_for_spawn_and_peer_stats() {
    let configured = profile("default", 600, 400);
    let cookbook = Cookbook {
        models: vec![Model {
            name: "model".into(),
            description: None,
            enabled: true,
            profiles: vec![configured.clone()],
        }],
    };
    let state = test_state_with_cookbook(test_config(), cookbook).await;
    let (pre_args, _, _) = build_pre_args(&configured);
    let args_hash = compute_args_hash(&pre_args);
    let instance = Instance::new(
        "ready-rss-only".into(),
        "model".into(),
        "default".into(),
        "127.0.0.1".into(),
        0,
        args_hash.clone(),
        false,
    );
    let pid = attach_live_child(&instance);
    *instance.status.lock() = InstanceStatus::Ready;
    state
        .memory_sampler
        .set_process_memory_override(pid, None, Some(100));
    state
        .instances
        .write()
        .await
        .insert("ready-rss-only".into(), Arc::new(RwLock::new(instance)));

    state.update_peak_memory("model", "default").await;
    assert_eq!(
        state.get_memory_estimate(&args_hash, &configured).await,
        (600, 100),
        "positive RSS must not turn unavailable NVML VRAM into a learned zero"
    );

    let peer = state.get_self_peer_state().await;
    let stats = &peer.model_stats["model:default"];
    assert_eq!(stats.vram_mb, 600);
    assert_eq!(stats.sysmem_mb, 100);

    let removed = state
        .instances
        .write()
        .await
        .remove("ready-rss-only")
        .unwrap();
    removed.read().await.stop().await.unwrap();
}

#[tokio::test]
async fn retirement_keeps_unmapped_process_memory_until_confirmed_reaping() {
    let state = test_state(test_config()).await;
    let mut guard = state
        .spawn_reservations
        .reserve("retiring:default".into(), (600, 0), None);
    let memory = guard.memory();
    guard.handoff();
    drop(guard);

    // The old instance has left the map but its reaper still owns this lease.
    let blocked = state
        .plan_spawn(
            &HashMap::new(),
            "next",
            &profile("default", 500, 0),
            (500, 0),
        )
        .await;
    assert!(matches!(blocked, Err(NodeError::InsufficientResources)));

    memory.finish();
    let plan = state
        .plan_spawn(
            &HashMap::new(),
            "next",
            &profile("default", 500, 0),
            (500, 0),
        )
        .await
        .expect("confirmed reaping frees the reservation");
    assert!(plan.victims.is_empty());
}

async fn retirement_state() -> Arc<NodeState> {
    test_state_with_cookbook(
        test_config(),
        Cookbook {
            models: vec![Model {
                name: "waiting".into(),
                description: None,
                enabled: true,
                profiles: vec![profile("default", 1_000, 0)],
            }],
        },
    )
    .await
}

async fn mapped_victim(
    state: &NodeState,
    name: &str,
    vram: u64,
    trap_term: bool,
) -> (Arc<RwLock<Instance>>, tokio::process::ChildStdout, u32) {
    use tokio::io::AsyncReadExt;
    let mut guard = state
        .spawn_reservations
        .reserve(format!("{name}:default"), (vram, 0), None);
    let mut inst = Instance::new(
        guard.memory().id().into(),
        name.into(),
        "default".into(),
        "127.0.0.1".into(),
        0,
        name.into(),
        false,
    );
    inst.memory_reservation = Some(guard.memory());
    let mut child = tokio::process::Command::new("/bin/sh")
        .args([
            "-c",
            if trap_term {
                "trap 'printf term' TERM; printf ready; while :; do read -r line; done"
            } else {
                "printf ready; exec sleep 60"
            },
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .unwrap();
    let pid = child.id().unwrap();
    let mut stdout = child.stdout.take().unwrap();
    let mut ready = [0; 5];
    timeout(Duration::from_secs(1), stdout.read_exact(&mut ready))
        .await
        .unwrap()
        .unwrap();
    assert_eq!(&ready, b"ready");
    guard.memory().set_pid(pid);
    *inst.child.lock() = Some(child);
    let inst = Arc::new(RwLock::new(inst));
    state
        .instances
        .write()
        .await
        .insert(guard.memory().id().into(), inst.clone());
    guard.handoff();
    (inst, stdout, pid)
}

async fn retirement_waiter(state: &NodeState) -> oneshot::Receiver<PendingQueuePermit> {
    let (tx, rx) = oneshot::channel();
    state.queues.write().await.insert(
        "waiting:default".into(),
        VecDeque::from([QueueEntry { token: 17, tx }]),
    );
    rx
}

#[tokio::test]
async fn cancelled_capacity_stop_wakes_only_after_all_mapped_victims_release() {
    use tokio::io::AsyncReadExt;
    let state = retirement_state().await;
    let (first, mut stdout, pid) = mapped_victim(&state, "first", 500, true).await;
    first.write().await.last_activity = Instant::now() - Duration::from_secs(1);
    let (second, _, _) = mapped_victim(&state, "second", 500, false).await;
    let caller_state = state.clone();
    let caller = tokio::spawn(async move {
        caller_state
            .try_get_or_spawn(
                "contender",
                &profile("default", 1_000, 0),
                true,
                "test",
                false,
            )
            .await
    });
    let mut term = [0; 4];
    timeout(Duration::from_secs(1), stdout.read_exact(&mut term))
        .await
        .expect("first victim must reach graceful stop")
        .unwrap();
    assert_eq!(&term, b"term");
    assert!(state.instances.read().await.is_empty());
    let mut waiter = retirement_waiter(&state).await;
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    assert_eq!(state.spawn_reservations.snapshot().entries.len(), 2);
    assert!(matches!(
        waiter.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));

    // Keep stale Instance Arcs alive: a dropped caller must still stop later
    // victims, and finish must release memory without waiting for those Arcs.
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGKILL,
    )
    .unwrap();
    let result = timeout(Duration::from_secs(1), &mut waiter).await;
    // Also clean up the negative-control run, where the second victim is stranded.
    first.read().await.stop().await.unwrap();
    second.read().await.stop().await.unwrap();
    let mut permit = result
        .expect("mapped retirement must wake without a maintenance tick")
        .unwrap();
    assert!(state.spawn_reservations.snapshot().entries.is_empty());
    permit.release().await;
}

#[tokio::test]
async fn capacity_retirement_cancellation_boundaries_preserve_wakeup() {
    for before_cleanup in [true, false] {
        let state = retirement_state().await;
        let (stale, _, _) = mapped_victim(&state, "old", 1_000, false).await;
        let hook = if before_cleanup {
            &state.test_hooks.before_retire
        } else {
            &state.test_hooks.after_capacity_retire
        };
        let (entered, release) = install_pause(hook);
        let caller_state = state.clone();
        let caller = tokio::spawn(async move {
            caller_state
                .try_get_or_spawn(
                    "contender",
                    &profile("default", 1_000, 0),
                    true,
                    "test",
                    false,
                )
                .await
        });
        wait_for_hook(entered, "retirement boundary").await;
        let mut waiter = retirement_waiter(&state).await;
        assert!(matches!(
            waiter.try_recv(),
            Err(oneshot::error::TryRecvError::Empty)
        ));
        caller.abort();
        assert!(caller.await.unwrap_err().is_cancelled());
        let _ = release.send(());
        let mut permit = timeout(Duration::from_secs(1), waiter)
            .await
            .expect("cancellation at either boundary must wake the queue")
            .unwrap();
        assert!(state.spawn_reservations.snapshot().entries.is_empty());
        assert!(stale.read().await.child.lock().is_none());
        permit.release().await;
    }
}

#[tokio::test]
async fn successful_capacity_retirement_defers_wake_until_contender_admission() {
    let state = retirement_state().await;
    let (_stale, _, _) = mapped_victim(&state, "old", 1_000, false).await;
    let mut waiter = retirement_waiter(&state).await;
    let (entered, release) = install_pause(&state.test_hooks.after_capacity_retire);
    let (reserved, resume) = install_pause(&state.test_hooks.after_reserve);
    let caller_state = state.clone();
    let caller = tokio::spawn(async move {
        caller_state
            .try_get_or_spawn(
                "contender",
                &profile("default", 1_000, 0),
                true,
                "test",
                false,
            )
            .await
    });
    wait_for_hook(entered, "retired capacity before re-admission").await;
    assert!(state.spawn_reservations.snapshot().entries.is_empty());
    assert!(matches!(
        waiter.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    release.send(()).unwrap();
    wait_for_hook(reserved, "contender reservation").await;
    assert_eq!(
        state.spawn_reservations.profile_count("contender:default"),
        1
    );
    assert!(matches!(
        waiter.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    resume.send(()).unwrap();
    assert!(caller.await.unwrap().is_err()); // Deliberately nonexistent executable.
    let mut permit = timeout(Duration::from_secs(1), waiter)
        .await
        .unwrap()
        .unwrap();
    permit.release().await;
}

#[tokio::test]
async fn buffered_retirement_completion_notifies_when_receiver_is_cancelled() {
    let state = retirement_state().await;
    let (stale, _, _) = mapped_victim(&state, "old", 1_000, false).await;
    let mut waiter = retirement_waiter(&state).await;
    let (sent, release) = install_pause(&state.test_hooks.after_retirement_send);
    let mut instances = state.instances.clone().write_owned().await;
    let victims = instances
        .drain()
        .map(|(_, inst)| (inst, "capacity"))
        .collect();
    let completion = state.retire_instances(instances, victims, true);
    wait_for_hook(sent, "completion buffered before caller receives it").await;
    assert!(state.spawn_reservations.snapshot().entries.is_empty());
    assert!(matches!(
        waiter.try_recv(),
        Err(oneshot::error::TryRecvError::Empty)
    ));
    drop(completion);
    release.send(()).unwrap();
    let mut permit = timeout(Duration::from_secs(1), waiter)
        .await
        .unwrap()
        .unwrap();
    assert!(stale.read().await.child.lock().is_none());
    permit.release().await;
}

#[tokio::test]
async fn cancelled_crash_prune_waits_for_an_existing_reaper_before_waking() {
    let state = retirement_state().await;
    let (stale, _stdout, pid) = mapped_victim(&state, "old", 1_000, true).await;
    {
        let inst = stale.read().await;
        let mut stop = Box::pin(inst.stop());
        assert!(futures::poll!(stop.as_mut()).is_pending());
        // A different caller has detached the reaper. The mapped instance now
        // looks dead, but its retained memory still blocks queue admission.
    }
    let (entered, release) = install_pause(&state.test_hooks.before_retire);
    let caller_state = state.clone();
    let caller = tokio::spawn(async move {
        caller_state
            .try_get_or_spawn(
                "contender",
                &profile("default", 1_000, 0),
                true,
                "test",
                false,
            )
            .await
    });
    wait_for_hook(entered, "crash-prune removal").await;
    let waiter = retirement_waiter(&state).await;
    caller.abort();
    assert!(caller.await.unwrap_err().is_cancelled());
    release.send(()).unwrap();
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(pid as i32),
        nix::sys::signal::Signal::SIGKILL,
    )
    .unwrap();
    let mut permit = timeout(Duration::from_secs(1), waiter)
        .await
        .expect("stop with no child must still wait for the existing reaper's release")
        .unwrap();
    assert!(state.spawn_reservations.snapshot().entries.is_empty());
    permit.release().await;
}

#[tokio::test]
async fn abandoned_reservation_before_enqueue_self_wakes_waiter_and_evicts_idle_incumbent() {
    let state = test_state(test_config()).await;
    let hash = "incumbent-hash";
    state
        .metrics
        .get_hash_metrics(hash)
        .await
        .observe_memory(500, 0);
    let incumbent = Instance::new(
        "incumbent".into(),
        "incumbent".into(),
        "default".into(),
        "127.0.0.1".into(),
        0,
        hash.into(),
        false,
    );
    let incumbent_pid = attach_live_child(&incumbent);
    state
        .memory_sampler
        .set_process_memory_override(incumbent_pid, Some(500), Some(1));
    state
        .instances
        .write()
        .await
        .insert("incumbent".into(), Arc::new(RwLock::new(incumbent)));

    // The temporary 500 MiB admission promise makes a 600 MiB contender fail
    // even after considering the idle 500 MiB incumbent as a victim. Once the
    // promise disappears, the contender must still evict that incumbent.
    let blocker = state
        .spawn_reservations
        .reserve("blocker:default".into(), (500, 0), None);
    let requested = profile("default", 600, 0);
    let (entered, release) = install_pause(&state.test_hooks.before_enqueue);
    let waiter_state = state.clone();
    let waiter = tokio::spawn(async move {
        waiter_state
            .get_instance_for_model("next", &requested, true)
            .await
    });
    wait_for_hook(entered, "waiter pre-enqueue pause").await;

    // Capacity frees before the waiter is observable in the queue: its release
    // notification is necessarily lost, so only the post-enqueue recheck can wake it.
    drop(blocker);
    release.send(()).unwrap();
    let result = timeout(Duration::from_secs(2), waiter)
        .await
        .expect("waiter must self-wake rather than sleep until a periodic tick")
        .unwrap()
        .expect_err("replacement reaches deliberately nonexistent executable");
    assert!(!matches!(result, NodeError::QueueTimeout));
    assert!(state.instances.read().await.get("incumbent").is_none());
}

#[tokio::test]
async fn periodic_recheck_wakes_queue_when_external_vram_falls() {
    let requested = profile("default", 400, 0);
    let state = test_state_with_cookbook(
        test_config(),
        Cookbook {
            models: vec![Model {
                name: "next".into(),
                description: None,
                enabled: true,
                profiles: vec![requested.clone()],
            }],
        },
    )
    .await;
    state
        .memory_sampler
        .set_device_vram_override(Some(gpu_snapshot(800, 1_000)));
    let key = "next:default".to_string();
    let (tx, mut rx) = oneshot::channel();
    state.queues.write().await.insert(
        key,
        std::collections::VecDeque::from([QueueEntry { token: 7, tx }]),
    );

    state.wake_admissible_queues().await;
    assert!(timeout(Duration::from_millis(20), &mut rx).await.is_err());

    state
        .memory_sampler
        .set_device_vram_override(Some(gpu_snapshot(0, 1_000)));
    state.wake_admissible_queues().await;
    let mut permit = timeout(Duration::from_secs(1), &mut rx)
        .await
        .expect("periodic recheck should wake admitted waiter")
        .expect("queue sender should remain live");
    permit.release().await;
}

#[tokio::test]
async fn retry_pre_exec_resamples_external_vram_with_the_existing_lease() {
    let state = test_state(test_config()).await;
    state.memory_sampler.set_device_vram_sequence(vec![
        gpu_snapshot(0, 1_000),   // initial admission
        gpu_snapshot(0, 1_000),   // first pre-exec check
        gpu_snapshot(700, 1_000), // retry pre-exec check
    ]);
    let err = state
        .try_get_or_spawn("retry", &profile("default", 400, 0), false, "test", false)
        .await
        .expect_err("first exec fails, then retry must observe the new external usage");
    assert!(
        matches!(err, NodeError::InsufficientResources),
        "the retry must resample instead of reaching a second nonexistent exec: {err:?}"
    );
    wait_for_empty_reservations(&state).await;
}
