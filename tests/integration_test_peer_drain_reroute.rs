//! Real HTTP routing regressions for attributed drain responses. Peers advertise
//! once before admission; no gossip task or capacity pulse rescues a bad retry.
//! Includes a real draining process and a gate on its retained HTTP connection.
#![cfg(target_os = "linux")]

use axum::{
    body::Body, extract::Json, http::StatusCode, response::Response, routing::post, Router,
};
use nix::sys::{prctl::set_child_subreaper, signal, wait};
use nix::unistd::Pid;
use serde_json::{json, Value};
use std::os::unix::{fs::PermissionsExt, process::CommandExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc,
};
use std::time::{Duration, Instant};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

mod common;

const MODEL: &str = "drain-model:default";
const SSE: &str = "data: {\"choices\":[{\"delta\":{\"content\":\"ok\"}}]}\n\ndata: [DONE]\n\n";

struct ProxyGroup {
    child: Child,
    log: PathBuf,
}

impl Drop for ProxyGroup {
    fn drop(&mut self) {
        let group = Pid::from_raw(self.child.id() as i32);
        let _ = signal::killpg(group, signal::Signal::SIGKILL);
        let _ = self.child.wait();
        loop {
            match wait::waitpid(Pid::from_raw(-group.as_raw()), None) {
                Ok(_) | Err(nix::errno::Errno::EINTR) => continue,
                Err(nix::errno::Errno::ECHILD) => break,
                Err(error) => panic!("cannot reap test process group: {error}"),
            }
        }
        if std::thread::panicking() {
            eprintln!(
                "proxy log:\n{}",
                std::fs::read_to_string(&self.log).unwrap()
            );
        }
    }
}

#[derive(Clone)]
enum Reply {
    Drain(Value),
    Success,
    Error(StatusCode, &'static str),
    BrokenBody,
}

struct Peer {
    id: &'static str,
    base: String,
    hits: Arc<AtomicUsize>,
    released: Arc<AtomicBool>,
    release: Arc<tokio::sync::Notify>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for Peer {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl Peer {
    async fn start(id: &'static str, reply: Reply) -> Self {
        Self::start_with_gate(id, reply, true).await
    }

    async fn start_gated(id: &'static str, reply: Reply) -> Self {
        Self::start_with_gate(id, reply, false).await
    }

    async fn start_with_gate(id: &'static str, reply: Reply, initially_released: bool) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("test peer requires loopback sockets");
        let base = format!("http://{}", listener.local_addr().unwrap());
        let hits = Arc::new(AtomicUsize::new(0));
        let counter = hits.clone();
        let released = Arc::new(AtomicBool::new(initially_released));
        let release = Arc::new(tokio::sync::Notify::new());
        let released_handler = released.clone();
        let release_handler = release.clone();
        let app = Router::new().route(
            "/v1/chat/completions",
            post(move |Json(request): Json<Value>| {
                let reply = reply.clone();
                let counter = counter.clone();
                let released = released_handler.clone();
                let release = release_handler.clone();
                async move {
                    counter.fetch_add(1, Ordering::SeqCst);
                    loop {
                        let notified = release.notified();
                        if released.load(Ordering::SeqCst) {
                            break;
                        }
                        notified.await;
                    }
                    match reply {
                        Reply::Drain(origin) => {
                            let mut body =
                                json!({"error": {"type": "draining", "message": "peer draining"}});
                            if !origin.is_null() {
                                body["error"]["origin_node_id"] = origin;
                            }
                            Response::builder()
                                .status(503)
                                .header("content-type", "application/json")
                                .header("retry-after", "5")
                                .header("x-drain-fixture", "unchanged")
                                .body(Body::from(body.to_string()))
                                .unwrap()
                        }
                        Reply::Success if request["stream"] == true => Response::builder()
                            .header("content-type", "text/event-stream")
                            .body(Body::from(SSE))
                            .unwrap(),
                        Reply::Success => Response::builder()
                            .header("content-type", "application/json")
                            .body(Body::from(json!({"served_by": id}).to_string()))
                            .unwrap(),
                        Reply::Error(status, kind) => Response::builder()
                            .status(status)
                            .header("content-type", "application/json")
                            .body(Body::from(
                                json!({"error": {"type": kind, "message": "fixture"}}).to_string(),
                            ))
                            .unwrap(),
                        Reply::BrokenBody => {
                            // A real incomplete HTTP body, not a complete malformed drain.
                            let chunks = futures::stream::unfold(false, |sent| async move {
                                if sent {
                                    tokio::time::sleep(Duration::from_millis(50)).await;
                                    Some((Err(std::io::Error::other("fixture body aborted")), true))
                                } else {
                                    Some((Ok(bytes::Bytes::from_static(b"{\"error\":")), true))
                                }
                            });
                            Response::builder()
                                .status(503)
                                .body(Body::from_stream(chunks))
                                .unwrap()
                        }
                    }
                }
            }),
        );
        let task = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        Self {
            id,
            base,
            hits,
            released,
            release,
            task,
        }
    }

    fn hits(&self) -> usize {
        self.hits.load(Ordering::SeqCst)
    }

    fn release(&self) {
        self.released.store(true, Ordering::SeqCst);
        self.release.notify_waiters();
    }
}

/// `/healthz` stays 503 when the cookbook is empty or the llama-server binary
/// is missing. Either response means the proxy is accepting requests; only a
/// missing answer means it is not up yet.
async fn wait_for_accepting(base_url: &str) -> bool {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .unwrap();
    for _ in 0..60 {
        if client
            .get(format!("{base_url}/healthz"))
            .send()
            .await
            .is_ok()
        {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    false
}

#[derive(Clone, Copy)]
enum Local {
    Unknown,
    KnownUnavailable,
    Ready,
    StartupFailure,
}

struct Harness {
    // Drop child processes before deleting their configuration and log files.
    proxy: ProxyGroup,
    temp: tempfile::TempDir,
    base: String,
    client: reqwest::Client,
}

impl Harness {
    async fn start(local: Local, queue_ms: u64) -> Self {
        Self::start_with(local, queue_ms, |_, _, _| {}).await
    }

    async fn start_with(
        local: Local,
        queue_ms: u64,
        configure: impl FnOnce(&Path, &mut Value, &mut Value),
    ) -> Self {
        set_child_subreaper(true).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("routing integration test requires loopback sockets");
        let address = listener.local_addr().unwrap();
        let mock = Path::new(env!("CARGO_BIN_EXE_mock_llama_server"));
        let binary = match local {
            Local::KnownUnavailable => temp.path().join("missing-llama-server"),
            Local::StartupFailure => {
                let script = temp.path().join("fail-startup.sh");
                // The executable starts successfully, then a real mock load fails.
                // An unexecutable binary would exercise a different NodeError.
                std::fs::write(&script, format!(
                    "#!/bin/sh\ntouch '{}'\nexec '{}' \"$@\" --startup-ready-file '{}' --startup-fail-file '{}'\n",
                    temp.path().join("fail-load").display(), mock.display(),
                    temp.path().join("never-ready").display(), temp.path().join("fail-load").display()
                )).unwrap();
                std::fs::set_permissions(&script, std::fs::Permissions::from_mode(0o755)).unwrap();
                script
            }
            _ => mock.to_owned(),
        };
        let mut config = json!({
            "node_id": "front", "listen_addr": address.to_string(),
            "metrics_path": temp.path().join("metrics.json"),
            "max_vram_mb": 1048576, "max_sysmem_mb": 1048576,
            "max_instances_per_node": 1, "shutdown_grace_period_seconds": 1,
            "default_model": MODEL,
            "model_defaults": {
                "max_concurrent_requests_per_instance": 1,
                "max_queue_size_per_model": 10, "max_instances_per_model": 1,
                "max_wait_in_queue_ms": queue_ms
            },
            "llama_cpp": {"repo_url": "", "repo_path": ".", "build_path": ".",
                "binary_path": binary, "branch": "master", "build_args": [],
                "build_command_args": [], "auto_update_interval_seconds": 0, "enabled": false},
            // handle_gossip still accepts the explicit fixture advertisements;
            // disabling its loop ensures no later capacity notification exists.
            "cluster": {"enabled": false, "peers": [], "gossip_interval_seconds": 60,
                "noise": {"enabled": false}, "discovery": {"mdns": false}},
            "http": {"request_body_limit_bytes": 1048576, "idle_timeout_seconds": 60}
        });
        let mut cookbook = if matches!(local, Local::Unknown) {
            json!({"models": []})
        } else {
            json!({"models": [{"name": "drain-model", "profiles": [{
                "id": "default", "model_path": "fixture.gguf", "max_instances": 1,
                "idle_timeout_seconds": 60, "startup_timeout_seconds": 2,
                "llama_server_args": ""
            }]}]})
        };
        configure(temp.path(), &mut config, &mut cookbook);
        let config_path = temp.path().join("config.yaml");
        let cookbook_path = temp.path().join("cookbook.yaml");
        std::fs::write(&config_path, serde_yaml::to_string(&config).unwrap()).unwrap();
        std::fs::write(&cookbook_path, serde_yaml::to_string(&cookbook).unwrap()).unwrap();
        let log = temp.path().join("proxy.log");
        let output = std::fs::File::create(&log).unwrap();
        drop(listener);
        let child = Command::new(common::llamesh_binary(Path::new(env!(
            "CARGO_MANIFEST_DIR"
        ))))
        .arg("--config")
        .arg(config_path)
        .arg("--cookbook")
        .arg(cookbook_path)
        .current_dir(temp.path())
        .env_clear()
        .env("PATH", std::env::var_os("PATH").unwrap())
        .env("HOME", temp.path())
        .env("RUST_LOG", "info")
        .stdout(Stdio::from(output.try_clone().unwrap()))
        .stderr(Stdio::from(output))
        .process_group(0)
        .spawn()
        .unwrap();
        let harness = Self {
            proxy: ProxyGroup { child, log },
            temp,
            base: format!("http://{address}"),
            client: reqwest::Client::builder()
                .timeout(Duration::from_secs(25))
                .build()
                .unwrap(),
        };
        assert!(
            wait_for_accepting(&harness.base).await,
            "proxy did not accept requests"
        );
        harness
    }

    async fn advertise(&self, peer: &Peer, rank: usize) {
        self.advertise_address(peer.id, &peer.base, rank).await;
    }

    async fn advertise_address(&self, id: &str, base: &str, rank: usize) {
        let response = self
            .client
            .post(format!("{}/cluster/gossip", self.base))
            .json(&json!({
                "origin": {
                    "node_id": id, "address": base, "version": env!("CARGO_PKG_VERSION"),
                    "last_seen": 0, "supported_models": ["drain-model"],
                    "model_stats": {"drain-model:default": {"tps": 200.0 - rank as f64 * 50.0,
                        "queue_len": rank, "instance_count": 1}},
                    "active_instances": 1, "max_instances": 1, "current_requests": 0,
                    "available_vram": 1048576, "available_sysmem": 1048576,
                    "max_vram": 1048576, "max_sysmem": 1048576,
                    "total_queue_length": rank, "ready": true, "loaded_models": [MODEL]
                }, "known_peers": []
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
    }

    async fn chat(&self, stream: bool, prefer_local: bool) -> reqwest::Response {
        let mut request = self.client.post(format!("{}/v1/chat/completions", self.base))
            .json(&json!({"model": MODEL, "messages": [{"role": "user", "content": "hi"}], "stream": stream}));
        if prefer_local {
            request = request.header("x-llama-mesh-hops", "1");
        }
        request.send().await.unwrap()
    }

    async fn clean_metrics(&self, requests: u64, errors: u64) {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let metrics: Value = self
                    .client
                    .get(format!("{}/metrics/json", self.base))
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                if metrics["current_requests"] == 0 {
                    assert_eq!(metrics["requests_total"], requests);
                    assert_eq!(metrics["errors_total"], errors);
                    assert_eq!(metrics["queue_pending_tokens"], 0);
                    return;
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("request cleanup did not finish");
    }

    async fn assert_ready(&self, peers: &[&Peer]) {
        let nodes: Value = self
            .client
            .get(format!("{}/cluster/nodes", self.base))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        for peer in peers {
            assert_eq!(nodes["nodes"][peer.id]["ready"], true);
        }
    }

    async fn finish_failed_startups(&self, peer: &Peer) {
        // Startup recovery has its own capacity waits. Wake only that phase;
        // after spawn_failures_exhausted no fixture gossip is sent again.
        tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                let log = std::fs::read_to_string(&self.proxy.log).unwrap();
                let events: Vec<Value> = log.lines()
                    .filter_map(|line| serde_json::from_str(line).ok()).collect();
                if let Some(exhausted) = events.iter().find(|event| event["fields"]["event"] == "spawn_failures_exhausted") {
                    assert_eq!(exhausted["fields"]["failures"], 3);
                    assert_eq!(events.iter().filter(|event| event["fields"]["event"] == "instance_spawn").count(), 3,
                        "fallback must follow three successfully executed processes whose loads failed");
                    peer.release();
                    return;
                }
                self.advertise(peer, 0).await;
                tokio::time::sleep(Duration::from_millis(50)).await;
            }
        })
        .await
        .expect("three actual startup failures never reached fallback");
        assert!(self
            .temp
            .path()
            .join("never-ready")
            .try_exists()
            .is_ok_and(|exists| !exists));
    }
}

/// An HTTP/1 TCP gate, preserving both ends of the warmed connection. It holds
/// the second complete request before B receives any of its bytes. Responses
/// pass through unchanged and are retained as wire evidence for the assertion.
struct ConnectionGate {
    connections: Arc<AtomicUsize>,
    requests: Arc<AtomicUsize>,
    release: Arc<tokio::sync::Notify>,
    responses: Arc<std::sync::Mutex<Vec<u8>>>,
    sockets: Arc<std::sync::Mutex<Vec<(std::net::SocketAddr, std::net::SocketAddr)>>>,
    task: tokio::task::JoinHandle<()>,
}

impl Drop for ConnectionGate {
    fn drop(&mut self) {
        self.task.abort();
    }
}

impl ConnectionGate {
    fn start(listener: tokio::net::TcpListener, destination: String) -> Self {
        let connections = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(AtomicUsize::new(0));
        let release = Arc::new(tokio::sync::Notify::new());
        let responses = Arc::new(std::sync::Mutex::new(Vec::new()));
        let sockets = Arc::new(std::sync::Mutex::new(Vec::new()));
        let (connections_task, requests_task, release_task, responses_task, sockets_task) = (
            connections.clone(),
            requests.clone(),
            release.clone(),
            responses.clone(),
            sockets.clone(),
        );
        let task = tokio::spawn(async move {
            let mut tasks = tokio::task::JoinSet::new();
            loop {
                tokio::select! {
                    accepted = listener.accept() => {
                        let (client, source) = accepted.unwrap();
                        connections_task.fetch_add(1, Ordering::SeqCst);
                        let backend = tokio::net::TcpStream::connect(&destination).await.unwrap();
                        sockets_task.lock().unwrap().push((source, backend.local_addr().unwrap()));
                        let (client_read, mut client_write) = client.into_split();
                        let (mut backend_read, mut backend_write) = backend.into_split();
                        let (requests, release, responses) =
                            (requests_task.clone(), release_task.clone(), responses_task.clone());
                        tasks.spawn(async move {
                            let upload = async move {
                                let mut reader = BufReader::new(client_read);
                                loop {
                                    let mut request = Vec::new();
                                    let mut content_length = 0;
                                    loop {
                                        let mut line = Vec::new();
                                        if reader.read_until(b'\n', &mut line).await.unwrap() == 0 { return; }
                                        if let Some(length) = String::from_utf8_lossy(&line).to_ascii_lowercase()
                                            .strip_prefix("content-length:") {
                                            content_length = length.trim().parse::<usize>().unwrap();
                                        }
                                        let end = line == b"\r\n";
                                        request.extend(line);
                                        if end { break; }
                                    }
                                    let start = request.len();
                                    request.resize(start + content_length, 0);
                                    reader.read_exact(&mut request[start..]).await.unwrap();
                                    if requests.fetch_add(1, Ordering::SeqCst) == 1 {
                                        release.notified().await;
                                    }
                                    if backend_write.write_all(&request).await.is_err() { return; }
                                }
                            };
                            let download = async move {
                                let mut bytes = [0; 8192];
                                loop {
                                    let count = backend_read.read(&mut bytes).await.unwrap();
                                    if count == 0 { return; }
                                    responses.lock().unwrap().extend_from_slice(&bytes[..count]);
                                    if client_write.write_all(&bytes[..count]).await.is_err() { return; }
                                }
                            };
                            // Ending either half drops the other half too.
                            tokio::select! { () = upload => {}, () = download => {} }
                        });
                    }
                    Some(result) = tasks.join_next(), if !tasks.is_empty() => { result.unwrap(); }
                }
            }
        });
        Self {
            connections,
            requests,
            release,
            responses,
            sockets,
            task,
        }
    }
}

async fn wait_until(mut condition: impl AsyncFnMut() -> bool, message: &str) {
    tokio::time::timeout(Duration::from_secs(5), async {
        while !condition().await {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("{message}"));
}

#[tokio::test]
async fn retained_peer_connection_reroutes_after_real_drain_gossip_already_arrived() {
    for stream in [false, true] {
        let front = Harness::start(Local::Unknown, 10000).await;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let gate_base = format!("http://{}", listener.local_addr().unwrap());
        let draining = Harness::start_with(Local::Ready, 30000, |directory, config, cookbook| {
            config["node_id"] = json!("draining-b");
            config["public_url"] = json!(gate_base);
            config["shutdown_grace_period_seconds"] = json!(30);
            config["cluster"]["enabled"] = json!(true);
            config["cluster"]["peers"] = json!([front.base]);
            config["cluster"]["gossip_interval_seconds"] = json!(300);
            let response_gate = directory.join("response-ready");
            std::fs::write(&response_gate, []).unwrap();
            cookbook["models"][0]["profiles"][0]["llama_server_args"] =
                json!(format!("--response-ready-file {}", response_gate.display()));
        })
        .await;
        let gate = ConnectionGate::start(
            listener,
            draining.base.strip_prefix("http://").unwrap().to_owned(),
        );
        front.advertise_address("draining-b", &gate_base, 0).await;
        let warm = front.chat(false, false).await;
        assert_eq!(warm.status(), StatusCode::OK);
        warm.bytes().await.unwrap();
        assert_eq!(gate.connections.load(Ordering::SeqCst), 1);
        assert_eq!(gate.requests.load(Ordering::SeqCst), 1);

        // B uses ordered gossip: wait for its real loaded snapshot. Its queue
        // is then 0 and C's is 1, deterministically selecting B even when real
        // gossip replaces the original fixture's performance statistics.
        wait_until(
            async || {
                let nodes: Value = front
                    .client
                    .get(format!("{}/cluster/nodes", front.base))
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                nodes["nodes"]["draining-b"]["loaded_models"]
                    .as_array()
                    .is_some_and(|models| models.iter().any(|model| model == MODEL))
            },
            "A never processed B's loaded-model gossip",
        )
        .await;

        // A real admitted request keeps B alive after SIGTERM. Its mock was
        // already loaded by the warmup, so no startup can obscure the race.
        std::fs::remove_file(draining.temp.path().join("response-ready")).unwrap();
        let held = draining.chat(false, true);
        tokio::pin!(held);
        let prepare = async {
            wait_until(
                async || {
                    let metrics: Value = draining
                        .client
                        .get(format!("{}/metrics/json", draining.base))
                        .send()
                        .await
                        .unwrap()
                        .json()
                        .await
                        .unwrap();
                    metrics["current_requests"] == 1
                },
                "B never admitted the held request",
            )
            .await;
            let healthy = Peer::start("healthy-c", Reply::Success).await;
            front.advertise(&healthy, 1).await;
            let request = front.chat(stream, false);
            tokio::pin!(request);
            let release = async {
                wait_until(
                    async || gate.requests.load(Ordering::SeqCst) == 2,
                    "second request never reached the warmed gate",
                )
                .await;
                assert_eq!(
                    gate.connections.load(Ordering::SeqCst),
                    1,
                    "A did not reuse its pooled connection"
                );
                signal::kill(
                    Pid::from_raw(draining.proxy.child.id() as i32),
                    signal::Signal::SIGTERM,
                )
                .unwrap();
                wait_until(
                    async || {
                        let nodes: Value = front
                            .client
                            .get(format!("{}/cluster/nodes", front.base))
                            .send()
                            .await
                            .unwrap()
                            .json()
                            .await
                            .unwrap();
                        nodes["nodes"]["draining-b"]["ready"] == false
                    },
                    "A never processed B's real drain gossip",
                )
                .await;
                wait_until(
                    async || {
                        tokio::net::TcpStream::connect(
                            draining.base.strip_prefix("http://").unwrap(),
                        )
                        .await
                        .is_err_and(|error| error.kind() == std::io::ErrorKind::ConnectionRefused)
                    },
                    "B kept accepting new connections",
                )
                .await;
                gate.release.notify_one();
            };
            let (response, ()) = tokio::time::timeout(Duration::from_secs(8), async {
                tokio::join!(&mut request, release)
            })
            .await
            .expect("retained-connection drain needed a later capacity wake");
            assert_eq!(response.status(), StatusCode::OK);
            let body = response.text().await.unwrap();
            if stream {
                assert_eq!(body, SSE);
            } else {
                assert_eq!(body, json!({"served_by": "healthy-c"}).to_string());
            }
            assert_eq!(healthy.hits(), 1);
            assert_eq!(gate.connections.load(Ordering::SeqCst), 1);
            assert_eq!(gate.requests.load(Ordering::SeqCst), 2);
            assert_eq!(
                gate.sockets.lock().unwrap().len(),
                1,
                "the B socket was replaced"
            );
            let wire = String::from_utf8(gate.responses.lock().unwrap().clone()).unwrap();
            assert!(
                wire.contains("503 Service Unavailable"),
                "B never rejected the retained request: {wire}"
            );
            assert!(
                wire.contains("\"origin_node_id\":\"draining-b\""),
                "B's actual drain lacked origin: {wire}"
            );
            front.clean_metrics(2, 0).await;
            std::fs::write(draining.temp.path().join("response-ready"), []).unwrap();
        };
        tokio::pin!(prepare);
        tokio::select! {
            _ = &mut held => panic!("B's held request ended before drain-race verification"),
            () = &mut prepare => {},
        }
        let response = held.await;
        assert_eq!(response.status(), StatusCode::OK);
        response.bytes().await.unwrap();
    }
}

#[tokio::test]
async fn known_and_unknown_models_reroute_draining_peer_without_a_wake() {
    for local in [Local::Unknown, Local::KnownUnavailable] {
        for stream in [false, true] {
            let rejected = Peer::start("draining-b", Reply::Drain(json!("draining-b"))).await;
            let healthy = Peer::start("healthy-c", Reply::Success).await;
            let harness = Harness::start(local, 5000).await;
            harness.advertise(&rejected, 0).await;
            harness.advertise(&healthy, 1).await;
            let response =
                tokio::time::timeout(Duration::from_secs(2), harness.chat(stream, false))
                    .await
                    .expect("drain rerouting waited for an unavailable capacity wake");
            assert_eq!(response.status(), StatusCode::OK);
            let body = response.text().await.unwrap();
            if stream {
                assert_eq!(body, SSE);
            } else {
                assert_eq!(body, json!({"served_by": "healthy-c"}).to_string());
            }
            assert_eq!(rejected.hits(), 1);
            assert_eq!(healthy.hits(), 1);
            harness.assert_ready(&[&rejected, &healthy]).await;
            harness.clean_metrics(1, 0).await;
        }
    }
}

#[tokio::test]
async fn draining_peer_reselects_local_without_an_http_self_forward() {
    for stream in [false, true] {
        let peer = Peer::start("draining-b", Reply::Drain(json!("draining-b"))).await;
        let harness = Harness::start(Local::Ready, 10000).await;
        harness.advertise(&peer, 0).await;
        let response = tokio::time::timeout(Duration::from_secs(6), harness.chat(stream, false))
            .await
            .expect("local rerouting waited for another capacity wake");
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.text().await.unwrap();
        if stream {
            assert!(body.contains("data: [DONE]"));
        } else {
            assert!(serde_json::from_str::<Value>(&body).unwrap()["choices"].is_array());
        }
        assert_eq!(peer.hits(), 1);
        // A self-forward starts a second admitted handler and increments this.
        harness.clean_metrics(1, 0).await;
    }
}

#[tokio::test]
async fn two_draining_peers_are_tried_once_then_original_queue_deadline_expires() {
    for local in [Local::Unknown, Local::KnownUnavailable] {
        let b = Peer::start("draining-b", Reply::Drain(json!("draining-b"))).await;
        let c = Peer::start("draining-c", Reply::Drain(json!("draining-c"))).await;
        let harness = Harness::start(local, 600).await;
        harness.advertise(&b, 0).await;
        harness.advertise(&c, 1).await;
        let started = Instant::now();
        let response = tokio::time::timeout(Duration::from_secs(3), harness.chat(false, false))
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
        let body: Value = response.json().await.unwrap();
        assert_eq!(body["error"]["type"], "queue_timeout");
        assert!(
            started.elapsed() >= Duration::from_millis(500),
            "exhaustion must wait, not produce a 404"
        );
        assert_eq!((b.hits(), c.hits()), (1, 1));
        harness.assert_ready(&[&b, &c]).await;
        harness.clean_metrics(1, 1).await;
    }
}

#[tokio::test]
async fn legacy_foreign_and_malformed_origins_surface_unchanged_on_each_request() {
    for origin in [Value::Null, json!("third-party"), json!(17)] {
        for stream in [false, true] {
            let peer = Peer::start("relay-b", Reply::Drain(origin.clone())).await;
            let harness = Harness::start(Local::Unknown, 5000).await;
            harness.advertise(&peer, 0).await;
            // More than the default breaker failure threshold. Every legacy
            // drain must remain neutral and the relay must remain selectable.
            for _ in 0..6 {
                let response =
                    tokio::time::timeout(Duration::from_secs(2), harness.chat(stream, false))
                        .await
                        .unwrap();
                assert_eq!(response.status(), StatusCode::SERVICE_UNAVAILABLE);
                assert_eq!(response.headers()["retry-after"], "5");
                assert_eq!(response.headers()["x-drain-fixture"], "unchanged");
                let mut expected =
                    json!({"error": {"type": "draining", "message": "peer draining"}});
                if !origin.is_null() {
                    expected["error"]["origin_node_id"] = origin.clone();
                }
                assert_eq!(response.text().await.unwrap(), expected.to_string());
            }
            assert_eq!(peer.hits(), 6);
            harness.assert_ready(&[&peer]).await;
            harness.clean_metrics(6, 0).await;
        }
    }
}

#[tokio::test]
async fn actual_spawn_failure_fallback_reroutes_drain_then_succeeds_once() {
    for stream in [false, true] {
        let b = Peer::start_gated("draining-b", Reply::Drain(json!("draining-b"))).await;
        let c = Peer::start("healthy-c", Reply::Success).await;
        let harness = Harness::start(Local::StartupFailure, 20000).await;
        harness.advertise(&b, 0).await;
        harness.advertise(&c, 1).await;
        let (response, ()) = tokio::join!(
            harness.chat(stream, true),
            harness.finish_failed_startups(&b)
        );
        assert_eq!(response.status(), StatusCode::OK);
        let body = response.text().await.unwrap();
        if stream {
            assert_eq!(body, SSE);
        } else {
            assert_eq!(body, json!({"served_by": "healthy-c"}).to_string());
        }
        assert_eq!((b.hits(), c.hits()), (1, 1));
        harness.clean_metrics(1, 0).await;
    }
}

#[tokio::test]
async fn actual_spawn_failure_fallback_preserves_exhaustion_and_error_policies() {
    for (reply, expected_status, expected_type) in [
        (
            Reply::Drain(json!("fallback-b")),
            StatusCode::SERVICE_UNAVAILABLE,
            "spawn_failures_exhausted",
        ),
        (
            Reply::Error(StatusCode::SERVICE_UNAVAILABLE, "queue_full"),
            StatusCode::SERVICE_UNAVAILABLE,
            "queue_full",
        ),
        (
            Reply::Error(StatusCode::BAD_REQUEST, "invalid_request_error"),
            StatusCode::BAD_REQUEST,
            "invalid_request_error",
        ),
        (Reply::BrokenBody, StatusCode::BAD_GATEWAY, "upstream_error"),
    ] {
        let peer = Peer::start_gated("fallback-b", reply).await;
        let harness = Harness::start(Local::StartupFailure, 20000).await;
        harness.advertise(&peer, 0).await;
        let (response, ()) = tokio::join!(
            harness.chat(false, true),
            harness.finish_failed_startups(&peer)
        );
        assert_eq!(response.status(), expected_status);
        assert_eq!(
            response.json::<Value>().await.unwrap()["error"]["type"],
            expected_type
        );
        assert_eq!(peer.hits(), 1);
        harness.clean_metrics(1, 1).await;
    }
}
