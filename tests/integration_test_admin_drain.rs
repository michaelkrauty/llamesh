#![cfg(target_os = "linux")]

use bytes::Bytes;
use http_body_util::{BodyExt, Full};
use hyper::{Request, StatusCode};
use hyper_util::rt::{TokioExecutor, TokioIo};
use nix::sys::{prctl::set_child_subreaper, signal, wait};
use nix::unistd::Pid;
use serde_json::{json, Value};
use std::{
    net::TcpListener,
    os::unix::process::CommandExt,
    path::Path,
    process::{Child, Command, Stdio},
    time::Duration,
};
use tokio::{net::TcpStream, time::timeout};

const DEADLINE: Duration = Duration::from_secs(15);

struct ProxyGroup(Child);

impl Drop for ProxyGroup {
    fn drop(&mut self) {
        let group = Pid::from_raw(self.0.id() as i32);
        let _ = signal::killpg(group, signal::Signal::SIGKILL);
        let _ = self.0.wait();
        loop {
            match wait::waitpid(Pid::from_raw(-group.as_raw()), None) {
                Ok(_) | Err(nix::errno::Errno::EINTR) => continue,
                Err(nix::errno::Errno::ECHILD) => break,
                Err(error) => panic!("Cannot reap isolated proxy group: {error}"),
            }
        }
    }
}

// Explicit senders cannot silently reconnect after the listener shuts down.
enum Connection {
    Http1(hyper::client::conn::http1::SendRequest<Full<Bytes>>),
    H2(hyper::client::conn::http2::SendRequest<Full<Bytes>>),
}

impl Connection {
    async fn open(address: std::net::SocketAddr, h2: bool) -> Self {
        let io = TokioIo::new(TcpStream::connect(address).await.unwrap());
        if h2 {
            let (sender, connection) =
                hyper::client::conn::http2::handshake(TokioExecutor::new(), io)
                    .await
                    .unwrap();
            tokio::spawn(async move {
                let _ = connection.await;
            });
            Self::H2(sender)
        } else {
            let (sender, connection) = hyper::client::conn::http1::handshake(io).await.unwrap();
            tokio::spawn(async move {
                let _ = connection.await;
            });
            Self::Http1(sender)
        }
    }

    async fn request(
        &mut self,
        path: &str,
        key: &str,
        body: Option<Value>,
    ) -> (StatusCode, hyper::HeaderMap, Value) {
        let request = Request::builder()
            .method(if body.is_some() { "POST" } else { "GET" })
            .uri(format!("http://localhost{path}"))
            .header("host", "localhost")
            .header("x-api-key", key)
            .header("content-type", "application/json")
            .body(Full::new(Bytes::from(
                body.map(|v| v.to_string()).unwrap_or_default(),
            )))
            .unwrap();
        timeout(DEADLINE, async {
            let response = match self {
                Self::Http1(sender) => sender.send_request(request).await,
                Self::H2(sender) => sender.send_request(request).await,
            }
            .expect("request on established connection failed");
            let (parts, body) = response.into_parts();
            let bytes = body.collect().await.unwrap().to_bytes();
            (
                parts.status,
                parts.headers,
                serde_json::from_slice(&bytes).unwrap(),
            )
        })
        .await
        .expect("established-connection request timed out")
    }
}

async fn admin_drain(h2: bool) {
    set_child_subreaper(true).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let mock = Path::new(env!("CARGO_MANIFEST_DIR")).join("target/release/mock_llama_server");
    assert!(
        mock.is_file(),
        "Build the release mock_llama_server before this test"
    );
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    // Reserve distinct dynamic ports until the proxy starts; never probe backends.
    let backend_listeners: Vec<_> = (0..2)
        .map(|_| TcpListener::bind("127.0.0.1:0").unwrap())
        .collect();
    let ranges: Vec<_> = backend_listeners
        .iter()
        .map(|listener| {
            let port = listener.local_addr().unwrap().port();
            json!({"start": port, "end": port})
        })
        .collect();
    let gate = dir.path().join("response-ready");
    let config = json!({
        "node_id": "admin-drain-test", "listen_addr": address.to_string(),
        "metrics_path": "metrics.json", "default_model": "warm",
        "max_vram_mb": 1048576, "max_sysmem_mb": 1048576,
        "max_instances_per_node": 2, "shutdown_grace_period_seconds": 60,
        "llama_cpp_ports": {"ranges": ranges},
        "model_defaults": {"max_instances_per_model": 1, "max_concurrent_requests_per_instance": 1,
            "max_queue_size_per_model": 10, "max_wait_in_queue_ms": 30000},
        "http": {"request_body_limit_bytes": 1048576, "idle_timeout_seconds": 60},
        "llama_cpp": {
            "enabled": false, "repo_url": "", "repo_path": ".", "build_path": ".",
            "binary_path": mock, "branch": "master", "build_args": [],
            "build_command_args": [], "auto_update_interval_seconds": 0
        },
        "cluster": {"enabled": false, "peers": [], "gossip_interval_seconds": 5,
            "discovery": {"mdns": false}, "noise": {"enabled": false}},
        "auth": {"enabled": true, "required_header": "x-api-key", "allowed_keys": ["secret"]}
    });
    let models: Vec<_> = ["warm", "cold"].into_iter().map(|name| json!({
        "name": name, "profiles": [{"id": "default", "model_path": format!("{name}.gguf"),
        "estimated_sysmem_mb": 1, "idle_timeout_seconds": 120,
        "llama_server_args": if name == "warm" { format!("--response-ready-file {}", gate.display()) } else { String::new() }}]
    })).collect();
    std::fs::write(
        dir.path().join("config.yaml"),
        serde_yaml::to_string(&config).unwrap(),
    )
    .unwrap();
    std::fs::write(
        dir.path().join("cookbook.yaml"),
        serde_yaml::to_string(&json!({"models": models})).unwrap(),
    )
    .unwrap();
    let log_path = dir.path().join("proxy.log");
    let log = std::fs::File::create(&log_path).unwrap();
    let binary = std::env::var_os("LLAMESH_ADMIN_DRAIN_BINARY")
        .unwrap_or_else(|| env!("CARGO_BIN_EXE_llamesh").into());
    drop(listener);
    drop(backend_listeners);
    let mut proxy = ProxyGroup(
        Command::new(binary)
            .args(["--config", "config.yaml", "--cookbook", "cookbook.yaml"])
            .current_dir(dir.path())
            .env_clear()
            .env("HOME", dir.path())
            .env("RUST_LOG", "info")
            .stdout(Stdio::from(log.try_clone().unwrap()))
            .stderr(Stdio::from(log))
            .process_group(0)
            .spawn()
            .unwrap(),
    );
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(DEADLINE)
        .build()
        .unwrap();
    timeout(DEADLINE, async {
        loop {
            assert!(
                proxy.0.try_wait().unwrap().is_none(),
                "proxy exited during startup: {}",
                std::fs::read_to_string(&log_path).unwrap()
            );
            if client
                .get(format!("http://{address}/healthz"))
                .send()
                .await
                .is_ok_and(|response| response.status().is_success())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("proxy startup timed out");
    let mut control = Connection::open(address, h2).await;
    for path in ["/admin/prewarm", "/admin/rebuild-llama"] {
        assert_eq!(
            control
                .request(path, "wrong", Some(json!({"model": "warm"})))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
    }
    assert_eq!(
        control
            .request("/admin/prewarm", "secret", Some(json!({"model": "warm"})))
            .await
            .0,
        StatusCode::OK
    );
    assert_eq!(
        control
            .request("/admin/rebuild-llama", "secret", Some(json!({})))
            .await
            .0,
        StatusCode::ACCEPTED
    );
    let inference = tokio::spawn(async move {
        client
            .post(format!("http://{address}/v1/chat/completions"))
            .header("x-api-key", "secret")
            .json(&json!({"model": "warm", "messages": [{"role": "user", "content": "hello"}]}))
            .send()
            .await
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()
    });
    timeout(DEADLINE, async {
        loop {
            let (_, _, nodes) = control.request("/cluster/nodes", "secret", None).await;
            if nodes["nodes"]["admin-drain-test"]["current_requests"] == 1 {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("held inference never acquired its slot");
    signal::kill(Pid::from_raw(proxy.0.id() as i32), signal::Signal::SIGTERM).unwrap();
    timeout(DEADLINE, async {
        loop {
            let (status, _, body) = control.request("/readyz", "secret", None).await;
            if status == StatusCode::SERVICE_UNAVAILABLE && body["status"] == "draining" {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("node never entered draining state");
    assert!(
        !inference.is_finished(),
        "inference must hold the shutdown window open"
    );
    for path in ["/admin/prewarm", "/admin/rebuild-llama"] {
        assert_eq!(
            control
                .request(path, "wrong", Some(json!({"model": "cold"})))
                .await
                .0,
            StatusCode::UNAUTHORIZED
        );
    }
    // Collect both before asserting, so baseline evidence includes both admissions.
    let prewarm = control
        .request("/admin/prewarm", "secret", Some(json!({"model": "cold"})))
        .await;
    let rebuild = control
        .request("/admin/rebuild-llama", "secret", Some(json!({})))
        .await;
    assert_eq!(
        (prewarm.0, rebuild.0),
        (
            StatusCode::SERVICE_UNAVAILABLE,
            StatusCode::SERVICE_UNAVAILABLE
        ),
        "post-drain admissions: prewarm={prewarm:?}, rebuild={rebuild:?}"
    );
    for (_, headers, body) in [prewarm, rebuild] {
        assert_eq!(headers["retry-after"], "5");
        assert_eq!(body["error"]["type"], "draining");
    }
    let (_, _, nodes) = control.request("/cluster/nodes", "secret", None).await;
    assert_eq!(nodes["nodes"]["admin-drain-test"]["active_instances"], 1);
    assert!(
        !std::fs::read_to_string(log_path)
            .unwrap()
            .lines()
            .any(|line| {
                serde_json::from_str::<Value>(line).is_ok_and(|event| {
                    event["fields"]["event"] == "instance_spawn"
                        && event["fields"]["model"] == "cold"
                })
            }),
        "draining prewarm spawned the cold model"
    );
    std::fs::write(gate, []).unwrap();
    let body = timeout(DEADLINE, inference).await.unwrap().unwrap();
    assert!(
        body["choices"][0]["message"]["content"].is_string(),
        "{body}"
    );
    timeout(DEADLINE, async {
        loop {
            if let Some(status) = proxy.0.try_wait().unwrap() {
                assert!(status.success());
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("proxy failed to finish draining");
}

#[tokio::test]
async fn http1_admin_admission_stops_when_draining() {
    admin_drain(false).await;
}

#[tokio::test]
async fn h2c_admin_admission_stops_when_draining() {
    admin_drain(true).await;
}
