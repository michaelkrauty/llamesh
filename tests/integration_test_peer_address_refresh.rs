//! Real discovery -> gossip -> inference after a peer changes listener port.
//! Requires Linux user/network/PID namespaces, `unshare`, `ip`, and `timeout`.
//! Build `target/release/mock_llama_server`, then run:
//! `cargo test --test integration_test_peer_address_refresh -- --nocapture`
#![cfg(target_os = "linux")]

use base64::Engine;
use nix::{sys::signal::Signal, unistd::Pid};
use serde_json::{json, Value};
use std::{
    fs,
    path::{Path, PathBuf},
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};

const PARENT_NETNS: &str = "LLAMESH_REFRESH_PARENT_NETNS";
const TEST_DIR: &str = "LLAMESH_REFRESH_TEST_DIR";
const ADDRESS: &str = "192.0.2.1";
const A_PORT: u16 = 19080;
const OLD_PORT: u16 = 19081;
const NEW_PORT: u16 = 19082;

#[test]
fn discovered_peer_port_refreshes_noise_route() {
    isolated_relocation(true, false);
}

#[test]
fn discovered_peer_port_refreshes_plaintext_route() {
    isolated_relocation(false, false);
}

#[test]
fn loopback_peer_port_refreshes_noise_route() {
    isolated_relocation(true, true);
}

#[test]
fn loopback_peer_port_refreshes_plaintext_route() {
    isolated_relocation(false, true);
}

fn isolated_relocation(noise: bool, loopback: bool) {
    let namespace = fs::read_link("/proc/self/ns/net").unwrap();
    let Ok(parent_namespace) = std::env::var(PARENT_NETNS) else {
        // Owned by the outer process so even the hard deadline cleans up files.
        let directory = tempfile::tempdir().unwrap();
        let test = std::thread::current().name().unwrap().to_owned();
        let output = Command::new("timeout")
            .args([
                "--signal=KILL",
                "90s",
                "unshare",
                "--user",
                "--map-root-user",
                "--net",
                "--pid",
                "--fork",
                "--kill-child",
            ])
            .arg(std::env::current_exe().unwrap())
            .args(["--exact", &test, "--nocapture"])
            .env(PARENT_NETNS, namespace)
            .env(TEST_DIR, directory.path())
            .output()
            .expect("install coreutils (timeout) and util-linux (unshare)");
        assert!(
            output.status.success(),
            "isolated relocation failed ({}); requires iproute2 and enabled user/network/PID namespaces:\n{}\n{}",
            output.status,
            String::from_utf8_lossy(&output.stdout),
            String::from_utf8_lossy(&output.stderr)
        );
        return;
    };

    // Fail closed before starting nodes or creating sockets. No host interfaces
    // enter this namespace. Its PID namespace also bounds descendant lifetime.
    assert_ne!(namespace.to_str().unwrap(), parent_namespace);
    for args in [
        vec!["link", "set", "lo", "up"],
        vec!["link", "add", "mesh-test0", "type", "dummy"],
        vec!["address", "add", "192.0.2.1/24", "dev", "mesh-test0"],
        vec!["link", "set", "mesh-test0", "multicast", "on", "up"],
        vec!["route", "add", "224.0.0.0/4", "dev", "mesh-test0"],
    ] {
        assert!(Command::new("ip").args(&args).status().unwrap().success());
    }
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .unwrap()
        .block_on(relocation(noise, loopback));
}

struct Node {
    child: Child,
    log: PathBuf,
}

impl Node {
    fn start(
        directory: &Path,
        id: &str,
        port: u16,
        noise: bool,
        token: &str,
        loopback: bool,
    ) -> Self {
        let root = Path::new(env!("CARGO_MANIFEST_DIR"));
        let binary = std::env::var_os("LLAMESH_ADDRESS_REFRESH_BINARY")
            .map(PathBuf::from)
            .unwrap_or_else(|| PathBuf::from(env!("CARGO_BIN_EXE_llamesh")));
        let backend = root.join("target/release/mock_llama_server");
        assert!(
            backend.is_file(),
            "build --release --bin mock_llama_server first"
        );
        let config = directory.join(format!("{id}.yaml"));
        let cookbook = directory.join(format!("{id}-cookbook.yaml"));
        // Non-loopback peers bootstrap through mDNS without seeds/public_url.
        // Loopback B seeds A; A learns B only from incoming gossip, so a fixed
        // configured B address cannot mask a stale source-derived route.
        let peers = if loopback && id == "node-b" {
            format!("[\"http://127.0.0.1:{A_PORT}\"]")
        } else {
            "[]".to_owned()
        };
        let listen = if loopback { "127.0.0.1" } else { "0.0.0.0" };
        let mdns = !loopback;
        // Omit enabled for Noise to exercise the production default.
        let plaintext = if noise { "" } else { "    enabled: false\n" };
        fs::write(
            &config,
            format!(
                r#"node_id: "{id}"
listen_addr: "{listen}:{port}"
metrics_path: "{id}-metrics.json"
shutdown_grace_period_seconds: 2
max_vram_mb: 1048576
max_sysmem_mb: 1048576
default_model: "remote-model:default"
model_defaults:
  max_concurrent_requests_per_instance: 2
  max_queue_size_per_model: 10
  max_instances_per_model: 1
  max_wait_in_queue_ms: 2000
llama_cpp_ports:
  ranges:
    - start: 19100
      end: 19109
llama_cpp:
  repo_url: ""
  repo_path: "."
  build_path: "."
  binary_path: "{}"
  branch: "master"
  build_args: []
  build_command_args: []
  auto_update_interval_seconds: 0
  enabled: false
cluster:
  enabled: true
  peers: {peers}
  gossip_interval_seconds: 1
  discovery:
    mdns: {mdns}
    service_name: "_refresh-test._tcp.local."
  noise:
{plaintext}    config_dir: "{id}-noise"
http:
  request_body_limit_bytes: 1048576
  idle_timeout_seconds: 5
"#,
                backend.display()
            ),
        )
        .unwrap();
        fs::write(
            &cookbook,
            if id == "node-a" {
                "models: []\n"
            } else {
                r#"models:
  - name: remote-model
    profiles:
      - id: default
        model_path: mock.gguf
        idle_timeout_seconds: 60
        max_instances: 1
        llama_server_args: ""
"#
            },
        )
        .unwrap();
        let log = directory.join(format!("{id}-{port}.log"));
        let output = fs::File::create(&log).unwrap();
        let child = Command::new(binary)
            .args([
                "--config",
                config.to_str().unwrap(),
                "--cookbook",
                cookbook.to_str().unwrap(),
            ])
            .current_dir(directory)
            .env_clear()
            .env("PATH", std::env::var_os("PATH").unwrap())
            .env("HOME", directory)
            .env("CLUSTER_TOKEN", token)
            .env("RUST_LOG", "info")
            .stdout(Stdio::from(output.try_clone().unwrap()))
            .stderr(Stdio::from(output))
            .spawn()
            .unwrap();
        Self { child, log }
    }

    async fn stop(&mut self) {
        nix::sys::signal::kill(Pid::from_raw(self.child.id() as i32), Signal::SIGTERM).unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(
                    status.success(),
                    "node did not shut down gracefully: {status}"
                );
                return;
            }
            assert!(Instant::now() < deadline, "graceful shutdown timed out");
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

impl Drop for Node {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if std::thread::panicking() {
            eprintln!(
                "{}:\n{}",
                self.log.display(),
                fs::read_to_string(&self.log).unwrap_or_default()
            );
        }
    }
}

async fn snapshot(client: &reqwest::Client, address: &str, port: u16) -> Value {
    match client
        .get(format!("http://{address}:{port}/cluster/nodes"))
        .send()
        .await
    {
        Ok(response) => response.json().await.unwrap_or(Value::Null),
        Err(_) => Value::Null,
    }
}

async fn wait_for_peer(client: &reqwest::Client, address: &str, expected_port: u16) -> Value {
    let deadline = Instant::now() + Duration::from_secs(25);
    loop {
        let nodes = snapshot(client, address, A_PORT).await;
        let peer_nodes = snapshot(client, address, expected_port).await;
        if (nodes["nodes"]["node-b"]["address"] == format!("http://{address}:{expected_port}")
            && nodes["nodes"]["node-b"]["ready"] == true
            && peer_nodes["nodes"]["node-a"]["address"] == format!("http://{address}:{A_PORT}"))
            || Instant::now() >= deadline
        {
            return nodes;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

async fn infer(client: &reqwest::Client, address: &str) -> Result<Value, String> {
    let response = client
        .post(format!("http://{address}:{A_PORT}/v1/chat/completions"))
        .json(&json!({"model": "remote-model", "messages": [{"role": "user", "content": "hello"}]}))
        .send()
        .await
        .map_err(|error| error.to_string())?;
    let status = response.status();
    let body: Value = response.json().await.map_err(|error| error.to_string())?;
    if !status.is_success()
        || body["choices"][0]["message"]["content"]
            != "This is a mock response from the dummy server."
    {
        return Err(format!("{status}: {body}"));
    }
    Ok(body)
}

async fn relocation(noise: bool, loopback: bool) {
    let address = if loopback { "127.0.0.1" } else { ADDRESS };
    let directory = PathBuf::from(std::env::var_os(TEST_DIR).unwrap());
    let token = base64::engine::general_purpose::STANDARD.encode(rand::random::<[u8; 32]>());
    let mut a = Node::start(&directory, "node-a", A_PORT, noise, &token, loopback);
    let mut b = Node::start(&directory, "node-b", OLD_PORT, noise, &token, loopback);
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let before = wait_for_peer(&client, address, OLD_PORT).await;
    assert_eq!(
        before["nodes"]["node-b"]["address"],
        format!("http://{address}:{OLD_PORT}"),
        "initial discovery/gossip failed: {before}"
    );
    assert_eq!(
        before["nodes"]["node-a"]["address"],
        format!("http://127.0.0.1:{A_PORT}")
    );
    let initial = infer(&client, address).await;
    assert!(
        initial.is_ok(),
        "initial inference through A must reach B: {initial:?}"
    );
    let key = noise.then(|| fs::read(directory.join("node-b-noise/node.key")).unwrap());
    b.stop().await;
    let mut b_new = Node::start(&directory, "node-b", NEW_PORT, noise, &token, loopback);
    let after = wait_for_peer(&client, address, NEW_PORT).await;
    let restarted = snapshot(&client, address, NEW_PORT).await;
    assert_eq!(
        restarted["nodes"]["node-b"]["address"],
        format!("http://127.0.0.1:{NEW_PORT}"),
        "restarted B must be healthy at its new listener: {restarted}"
    );
    let forwarded = infer(&client, address).await;
    assert!(
        a.child.try_wait().unwrap().is_none(),
        "A must remain running throughout relocation"
    );
    if let Some(key) = key {
        assert_eq!(
            fs::read(directory.join("node-b-noise/node.key")).unwrap(),
            key
        );
    }
    assert_eq!(
        after["nodes"]["node-b"]["address"],
        format!("http://{address}:{NEW_PORT}"),
        "A retained a stale peer address after B moved; inference through A: {forwarded:?}"
    );
    assert_eq!(
        restarted["nodes"]["node-a"]["address"],
        format!("http://{address}:{A_PORT}"),
        "restarted B must rediscover A: {restarted}"
    );
    assert!(
        forwarded.is_ok(),
        "inference through A must reach relocated B: {forwarded:?}"
    );
    b_new.stop().await;
    a.stop().await;
}
