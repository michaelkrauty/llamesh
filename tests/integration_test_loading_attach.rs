//! Requests that join, leave, or wait on an instance while it is still
//! loading must neither block the node nor change how the load is handled.

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

/// This node's `/cluster/nodes` entry. Fails if the node does not answer
/// within the client's timeout.
async fn node(client: &reqwest::Client, base: &str) -> Result<Value, reqwest::Error> {
    let body = client
        .get(format!("{base}/cluster/nodes"))
        .send()
        .await?
        .json::<Value>()
        .await?;
    Ok(body["nodes"]["loading-attach"].clone())
}

/// Number of logged events whose `fields.<key>` equals `value`.
async fn count_events(log: &Path, key: &str, value: &str) -> usize {
    tokio::fs::read_to_string(log)
        .await
        .unwrap()
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter(|event| event["fields"][key] == value)
        .count()
}

async fn wait_for_events(log: &Path, key: &str, value: &str, count: usize) {
    tokio::time::timeout(Duration::from_secs(15), async {
        while count_events(log, key, value).await < count {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .unwrap_or_else(|_| panic!("expected {count} `{key}: {value}` events"));
}

fn chat(client: &reqwest::Client, base: &str, timeout: Duration) -> reqwest::RequestBuilder {
    client
        .post(format!("{base}/v1/chat/completions"))
        .timeout(timeout)
        .json(&json!({
            "model": "model-a",
            "messages": [{"role": "user", "content": "Hello"}],
            "stream": false
        }))
}

/// A proxy serving one model whose mock instance reports loading until
/// `gate` exists, and fails its load once when `fail` is created.
struct Harness {
    _proxy: ProxyGroup,
    temp: tempfile::TempDir,
    base: String,
    output: PathBuf,
    gate: PathBuf,
    fail: PathBuf,
    client: reqwest::Client,
}

impl Harness {
    async fn start(slots: usize, idle_timeout_seconds: u64) -> Self {
        // Only this integration-test executable becomes a subreaper. Its
        // children have a dedicated process group, so cleanup never targets
        // other services.
        set_child_subreaper(true).unwrap();
        let temp = tempfile::tempdir().unwrap();
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let binary = std::env::var_os("LLAMESH_TEST_BINARY")
            .map(PathBuf::from)
            .unwrap_or_else(|| common::llamesh_binary(root));
        let mock = Path::new(env!("CARGO_BIN_EXE_mock_llama_server"));
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let address = listener.local_addr().unwrap();
        let gate = temp.path().join("startup-ready");
        let fail = temp.path().join("startup-fail");
        let config = json!({
            "node_id": "loading-attach",
            "listen_addr": address.to_string(),
            "metrics_path": temp.path().join("metrics.json"),
            "logging": {"enabled": true, "directory": temp.path().join("logs")},
            "max_vram_mb": 1048576,
            "max_sysmem_mb": 1048576,
            "default_model": "model-a",
            "model_defaults": {
                "max_concurrent_requests_per_instance": slots,
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
            "cluster": {"enabled": false, "peers": [], "gossip_interval_seconds": 5},
            "http": {"request_body_limit_bytes": 1048576, "idle_timeout_seconds": 60}
        });
        let cookbook = json!({"models": [{
            "name": "model-a",
            "profiles": [{
                "id": "default",
                "model_path": "model-a.gguf",
                "idle_timeout_seconds": idle_timeout_seconds,
                "startup_timeout_seconds": 60,
                "llama_server_args": format!(
                    "--parallel {slots} --startup-ready-file {} --startup-fail-file {}",
                    gate.display(),
                    fail.display()
                )
            }]
        }]});
        let config_path = temp.path().join("config.yaml");
        let cookbook_path = temp.path().join("cookbook.yaml");
        std::fs::write(&config_path, serde_yaml::to_string(&config).unwrap()).unwrap();
        std::fs::write(&cookbook_path, serde_yaml::to_string(&cookbook).unwrap()).unwrap();
        let output_path = temp.path().join("proxy-output.log");
        let output = std::fs::File::create(&output_path).unwrap();
        drop(listener);
        let proxy = ProxyGroup(
            Command::new(binary)
                .args([
                    "--config",
                    config_path.to_str().unwrap(),
                    "--cookbook",
                    cookbook_path.to_str().unwrap(),
                ])
                .current_dir(temp.path())
                .env("RUST_LOG", "info")
                .stdout(Stdio::from(output.try_clone().unwrap()))
                .stderr(Stdio::from(output))
                .process_group(0)
                .spawn()
                .unwrap(),
        );
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(2))
            .build()
            .unwrap();
        let base = format!("http://{address}");
        tokio::time::timeout(Duration::from_secs(15), async {
            while !node(&client, &base)
                .await
                .is_ok_and(|node| node["ready"] == true)
            {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await
        .expect("proxy never became ready");
        Harness {
            _proxy: proxy,
            temp,
            base,
            output: output_path,
            gate,
            fail,
            client,
        }
    }

    fn chat(
        &self,
        timeout: Duration,
    ) -> tokio::task::JoinHandle<reqwest::Result<reqwest::Response>> {
        tokio::spawn(chat(&self.client, &self.base, timeout).send())
    }

    async fn node(&self) -> Value {
        node(&self.client, &self.base)
            .await
            .expect("node stopped answering")
    }

    async fn wait_for_node(&self, predicate: impl Fn(&Value) -> bool) -> Value {
        let mut last = Value::Null;
        let found = tokio::time::timeout(Duration::from_secs(15), async {
            loop {
                last = self.node().await;
                if predicate(&last) {
                    return last.clone();
                }
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
        .await;
        found.unwrap_or_else(|_| panic!("node never reached the expected state: {last}"))
    }

    async fn events(&self, key: &str, value: &str) -> usize {
        count_events(&self.output, key, value).await
    }
}

async fn assert_ok(request: tokio::task::JoinHandle<reqwest::Result<reqwest::Response>>) {
    let response = request.await.unwrap().unwrap();
    let status = response.status();
    let body = response.text().await.unwrap();
    assert_eq!(status, reqwest::StatusCode::OK, "{body}");
}

const LONG: Duration = Duration::from_secs(45);

#[tokio::test]
async fn request_joining_a_loading_instance_does_not_block_the_node() {
    // Two requests share one instance: the second joins it while it loads.
    let h = Harness::start(2, 60).await;
    let a = h.chat(LONG);
    wait_for_events(&h.output, "event", "instance_spawn", 1).await;
    let b = h.chat(LONG);
    // B has chosen to serve locally; it joins the loading instance next.
    wait_for_events(&h.output, "message", "Route decision made", 2).await;
    tokio::time::sleep(Duration::from_millis(300)).await;

    // While the instance loads, the node keeps answering: B waits for the
    // instance to become ready without holding up anyone else.
    for _ in 0..5 {
        let node = node(&h.client, &h.base)
            .await
            .expect("node stopped answering while a joined instance was loading");
        assert_eq!(node["active_instances"], 1, "{node}");
        assert_eq!(node["current_requests"], 2, "{node}");
        assert_eq!(
            node["total_queue_length"], 0,
            "B must join, not queue: {node}"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }
    assert!(!a.is_finished() && !b.is_finished());

    std::fs::write(&h.gate, []).unwrap();
    assert_ok(a).await;
    assert_ok(b).await;
    assert_eq!(
        h.events("event", "instance_spawn").await,
        1,
        "both requests must be served by the one instance"
    );
}

#[tokio::test]
async fn load_continues_after_its_request_leaves() {
    // The request gives up while its instance loads. With a one-second idle
    // timeout, the instance must not be stopped as idle before it is ready:
    // the next request finds the finished load instead of restarting it.
    let h = Harness::start(1, 1).await;
    let gone = h.chat(Duration::from_secs(1));
    wait_for_events(&h.output, "event", "instance_spawn", 1).await;
    assert!(gone.await.unwrap().is_err(), "the request must time out");

    // Let the idle sweep, which runs every 10 seconds, pass at least once.
    tokio::time::sleep(Duration::from_secs(12)).await;
    let node = h.node().await;
    assert_eq!(node["active_instances"], 1, "the load was stopped: {node}");
    assert_eq!(h.events("event", "instance_terminate").await, 0);

    std::fs::write(&h.gate, []).unwrap();
    assert_ok(h.chat(LONG)).await;
    assert_eq!(h.events("event", "instance_spawn").await, 1);
}

#[tokio::test]
async fn failed_load_is_handled_once_by_its_waiting_requests() {
    // Three requests wait on a cold load that fails. One of them runs the
    // cold-start recovery; the others retry, and all are served by the
    // replacement instance.
    let h = Harness::start(3, 60).await;
    let mut requests = vec![h.chat(LONG)];
    // The others join the loading instance once it is in the instance map.
    wait_for_events(&h.output, "event", "instance_spawn", 1).await;
    requests.extend((0..2).map(|_| h.chat(LONG)));
    h.wait_for_node(|node| {
        node["current_requests"] == 3
            && node["active_instances"] == 1
            && node["total_queue_length"] == 0
    })
    .await;

    std::fs::write(&h.fail, []).unwrap();
    wait_for_events(&h.output, "event", "instance_spawn", 2).await;
    std::fs::write(&h.gate, []).unwrap();
    for request in requests {
        assert_ok(request).await;
    }
    assert_eq!(
        h.events("event", "cold_oom_retry").await,
        1,
        "only one request may run cold-start recovery for the failed load"
    );
    assert_eq!(h.events("event", "instance_spawn").await, 2);
    drop(h.temp);
}
