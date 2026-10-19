//! A node that starts draining must tell its peers at once and stop
//! accepting connections, so they fail over instead of sending it work.

use nix::sys::{prctl::set_child_subreaper, signal, wait};
use nix::unistd::Pid;
use serde_json::{json, Value};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::Duration;

mod common;

/// Isolated process group, including orphaned mocks if the proxy crashes.
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
                Err(error) => panic!("Cannot reap isolated mock process group: {error}"),
            }
        }
    }
}

/// Far longer than the test runs: peers learn of a drain only from the
/// gossip that the drain itself triggers, never from a periodic round.
const GOSSIP_INTERVAL_SECONDS: u64 = 300;

fn start_node(
    dir: &Path,
    node_id: &str,
    address: std::net::SocketAddr,
    peer: std::net::SocketAddr,
    llama_server_args: &str,
) -> ProxyGroup {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let binary = std::env::var_os("LLAMESH_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| common::llamesh_binary(root));
    let mock = Path::new(env!("CARGO_BIN_EXE_mock_llama_server"));
    let dir = dir.join(node_id);
    std::fs::create_dir(&dir).unwrap();
    let config = json!({
        "node_id": node_id,
        "listen_addr": address.to_string(),
        "metrics_path": dir.join("metrics.json"),
        "logging": {"enabled": true, "directory": dir.join("logs")},
        "max_vram_mb": 1048576,
        "max_sysmem_mb": 1048576,
        "default_model": "held",
        "shutdown_grace_period_seconds": 60,
        "model_defaults": {
            "max_concurrent_requests_per_instance": 1,
            "max_queue_size_per_model": 10,
            "max_instances_per_model": 1,
            "max_wait_in_queue_ms": 30000
        },
        "llama_cpp": {
            "repo_url": "", "repo_path": ".", "build_path": ".",
            "binary_path": mock, "branch": "master", "build_args": [],
            "build_command_args": [], "auto_update_interval_seconds": 0,
            "enabled": false
        },
        "cluster": {
            "enabled": true,
            "peers": [format!("http://{peer}")],
            "gossip_interval_seconds": GOSSIP_INTERVAL_SECONDS,
            "discovery": {"mdns": false},
            "noise": {"enabled": false}
        },
        "http": {"request_body_limit_bytes": 1048576, "idle_timeout_seconds": 60}
    });
    let cookbook = json!({"models": [{
        "name": "held",
        "profiles": [{
            "id": "default",
            "model_path": "held.gguf",
            "idle_timeout_seconds": 60,
            "llama_server_args": llama_server_args
        }]
    }]});
    std::fs::write(
        dir.join("config.yaml"),
        serde_yaml::to_string(&config).unwrap(),
    )
    .unwrap();
    std::fs::write(
        dir.join("cookbook.yaml"),
        serde_yaml::to_string(&cookbook).unwrap(),
    )
    .unwrap();
    let output = std::fs::File::create(dir.join("proxy-output.log")).unwrap();
    ProxyGroup(
        Command::new(binary)
            .args(["--config", "config.yaml", "--cookbook", "cookbook.yaml"])
            .current_dir(&dir)
            .env("HOME", &dir)
            .env("RUST_LOG", "info")
            .stdout(Stdio::from(output.try_clone().unwrap()))
            .stderr(Stdio::from(output))
            .process_group(0)
            .spawn()
            .unwrap(),
    )
}

/// `node_id`'s entry in `observer`'s `/cluster/nodes`, once `predicate`
/// holds for it.
async fn wait_for_view(
    client: &reqwest::Client,
    observer: std::net::SocketAddr,
    node_id: &str,
    within: Duration,
    predicate: impl Fn(&Value) -> bool,
) -> Result<Value, Value> {
    let mut last = Value::Null;
    let found = tokio::time::timeout(within, async {
        loop {
            if let Ok(response) = client
                .get(format!("http://{observer}/cluster/nodes"))
                .send()
                .await
            {
                if let Ok(body) = response.json::<Value>().await {
                    last = body["nodes"][node_id].clone();
                    if predicate(&last) {
                        return last.clone();
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    found.map_err(|_| last)
}

/// Two meshed nodes, A and B, each serving `held`. B's instance holds chat
/// responses until `gate` exists.
struct Pair {
    _temp: tempfile::TempDir,
    _a_proxy: ProxyGroup,
    b_proxy: ProxyGroup,
    a: std::net::SocketAddr,
    b: std::net::SocketAddr,
    gate: PathBuf,
    client: reqwest::Client,
}

async fn start_pair() -> Pair {
    // Only this integration-test executable becomes a subreaper. Its children
    // have dedicated process groups, so cleanup never targets other services.
    set_child_subreaper(true).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let listeners: Vec<_> = (0..2)
        .map(|_| std::net::TcpListener::bind("127.0.0.1:0").unwrap())
        .collect();
    let a = listeners[0].local_addr().unwrap();
    let b = listeners[1].local_addr().unwrap();
    drop(listeners);
    let gate = temp.path().join("response-ready");
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(2))
        .build()
        .unwrap();

    // A starts first, so B's first gossip round, sent at startup, reaches it.
    let a_proxy = start_node(temp.path(), "drain-a", a, b, "");
    wait_for_view(&client, a, "drain-a", Duration::from_secs(15), |node| {
        node["ready"] == true
    })
    .await
    .expect("A never became ready");
    let b_proxy = start_node(
        temp.path(),
        "drain-b",
        b,
        a,
        &format!("--response-ready-file {}", gate.display()),
    );
    wait_for_view(&client, a, "drain-b", Duration::from_secs(15), |node| {
        node["ready"] == true
    })
    .await
    .expect("A never saw B ready");
    Pair {
        _temp: temp,
        _a_proxy: a_proxy,
        b_proxy,
        a,
        b,
        gate,
        client,
    }
}

async fn wait_for_exit(proxy: &mut ProxyGroup) -> std::process::ExitStatus {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(status) = proxy.0.try_wait().unwrap() {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("node never finished draining")
}

#[tokio::test]
async fn idle_draining_node_tells_peers_before_it_exits() {
    let mut pair = start_pair().await;
    let (a, client) = (pair.a, pair.client.clone());

    // B has nothing in flight, so its drain finishes at once. It must still
    // send the gossip round that tells A it is no longer ready before it
    // exits.
    signal::kill(
        Pid::from_raw(pair.b_proxy.0.id() as i32),
        signal::Signal::SIGTERM,
    )
    .unwrap();
    assert!(wait_for_exit(&mut pair.b_proxy).await.success());
    let view = wait_for_view(&client, a, "drain-b", Duration::from_secs(2), |_| true)
        .await
        .expect("A did not answer");
    assert_eq!(
        view["ready"], false,
        "A still sees B as ready after B exited: {view}"
    );
}

#[tokio::test]
async fn draining_node_tells_peers_and_refuses_new_connections() {
    let mut pair = start_pair().await;
    let (a, b, client) = (pair.a, pair.b, pair.client.clone());

    // A request held on B keeps B draining instead of exiting.
    let held = tokio::spawn(
        client
            .post(format!("http://{b}/v1/chat/completions"))
            .header("x-llama-mesh-hops", "1")
            .timeout(Duration::from_secs(45))
            .json(&json!({
                "model": "held",
                "messages": [{"role": "user", "content": "Hello"}],
                "stream": false
            }))
            .send(),
    );
    // Wait until A has the gossip B sent when the held request's instance
    // became ready, so that the drain is the only state change left to
    // trigger B's next gossip round.
    wait_for_view(&client, a, "drain-b", Duration::from_secs(15), |node| {
        node["current_requests"] == 1
            && node["loaded_models"]
                .as_array()
                .is_some_and(|models| models.iter().any(|model| model == "held:default"))
    })
    .await
    .expect("A never saw the held request on B's ready instance");

    signal::kill(
        Pid::from_raw(pair.b_proxy.0.id() as i32),
        signal::Signal::SIGTERM,
    )
    .unwrap();

    // A learns that B is draining well before its next periodic gossip.
    wait_for_view(&client, a, "drain-b", Duration::from_secs(5), |node| {
        node["ready"] == false
    })
    .await
    .unwrap_or_else(|last| panic!("A still sees B as ready after it began draining: {last}"));

    // B no longer accepts connections; a new one is refused at once instead
    // of waiting unanswered in the accept backlog.
    let refused = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match tokio::net::TcpStream::connect(b).await {
                Err(error) if error.kind() == std::io::ErrorKind::ConnectionRefused => break,
                _ => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
    })
    .await;
    assert!(
        refused.is_ok(),
        "B kept accepting connections while draining"
    );
    assert!(!held.is_finished(), "the held request must keep B draining");

    // The request in flight when the drain began still completes.
    std::fs::write(&pair.gate, []).unwrap();
    let response = held.await.unwrap().unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    let status = wait_for_exit(&mut pair.b_proxy).await;
    assert!(status.success());
}
