//! A loading process must retain its estimated memory after map insertion.

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

async fn wait_for_node(
    client: &reqwest::Client,
    base: &str,
    predicate: impl Fn(&Value) -> bool,
) -> Value {
    let mut last = Value::Null;
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Ok(response) = client.get(format!("{base}/cluster/nodes")).send().await {
                if let Ok(body) = response.json::<Value>().await {
                    last = body["nodes"]["memory-guardrail"].clone();
                    if predicate(&last) {
                        return last.clone();
                    }
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    result.unwrap_or_else(|_| panic!("Proxy did not reach expected state; last node: {last}"))
}

fn chat(client: &reqwest::Client, base: &str, model: &str) -> reqwest::RequestBuilder {
    client
        .post(format!("{base}/v1/chat/completions"))
        .timeout(Duration::from_secs(45))
        .json(&json!({
            "model": model,
            "messages": [{"role": "user", "content": "Hello"}],
            "stream": false
        }))
}

async fn wait_for_spawn(log: &Path, model: &str) {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let output = tokio::fs::read_to_string(log).await.unwrap();
            if output.lines().any(|line| {
                serde_json::from_str::<Value>(line).is_ok_and(|event| {
                    event["fields"]["event"] == "instance_spawn"
                        && event["fields"]["model"] == model
                })
            }) {
                return;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("instance_spawn must confirm the loading process reached the map");
}

#[tokio::test]
async fn loading_commitment_queues_other_model_until_readiness() {
    // Only this integration-test executable becomes a subreaper. Its children
    // have a dedicated process group, so cleanup never targets other services.
    set_child_subreaper(true).unwrap();
    let temp = tempfile::tempdir().unwrap();
    let root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let binary = std::env::var_os("LLAMESH_TEST_BINARY")
        .map(PathBuf::from)
        .unwrap_or_else(|| common::llamesh_binary(root));
    let mock = Path::new(env!("CARGO_BIN_EXE_mock_llama_server"));
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let address = listener.local_addr().unwrap();
    let base = format!("http://{address}");
    let gate = temp.path().join("startup-ready");
    let response_gate = temp.path().join("response-ready");
    let config = json!({
        "node_id": "memory-guardrail",
        "listen_addr": address.to_string(),
        "metrics_path": temp.path().join("metrics.json"),
        "logging": {"enabled": true, "directory": temp.path().join("logs")},
        "max_vram_mb": 1048576,
        "max_sysmem_mb": 1000,
        "max_instances_per_node": 2,
        "default_model": "model-a",
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
        "cluster": {"enabled": false, "peers": [], "gossip_interval_seconds": 5},
        "http": {"request_body_limit_bytes": 1048576, "idle_timeout_seconds": 60}
    });
    let models: Vec<_> = ["model-a", "model-b"]
        .into_iter()
        .map(|model| {
            let mut args = format!("--startup-ready-file {}", gate.display());
            if model == "model-a" {
                args.push_str(&format!(
                    " --response-ready-file {}",
                    response_gate.display()
                ));
            }
            json!({
                "name": model,
                "profiles": [{
                    "id": "default",
                    "model_path": format!("{model}.gguf"),
                    "estimated_sysmem_mb": 600,
                    "idle_timeout_seconds": 60,
                    "startup_timeout_seconds": 60,
                    "llama_server_args": args
                }]
            })
        })
        .collect();
    let config_path = temp.path().join("config.yaml");
    let cookbook_path = temp.path().join("cookbook.yaml");
    std::fs::write(&config_path, serde_yaml::to_string(&config).unwrap()).unwrap();
    std::fs::write(
        &cookbook_path,
        serde_yaml::to_string(&json!({"models": models})).unwrap(),
    )
    .unwrap();
    let output_path = temp.path().join("proxy-output.log");
    let output = std::fs::File::create(&output_path).unwrap();
    drop(listener);
    let _proxy = ProxyGroup(
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
    wait_for_node(&client, &base, |node| node["ready"] == true).await;
    let a = tokio::spawn(chat(&client, &base, "model-a").send());
    wait_for_node(&client, &base, |node| node["active_instances"] == 1).await;
    wait_for_spawn(&output_path, "model-a").await;
    let b = tokio::spawn(chat(&client, &base, "model-b").send());
    let blocked = wait_for_node(&client, &base, |node| {
        node["total_queue_length"].as_u64().unwrap_or(0) > 0
            || node["active_instances"].as_u64().unwrap_or(0) > 1
    })
    .await;
    assert_eq!(
        blocked["active_instances"], 1,
        "Oversubscribed while model-a was loading: {blocked}"
    );
    assert!(blocked["total_queue_length"].as_u64().unwrap() > 0);
    assert!(!gate.exists());
    std::fs::write(&gate, []).unwrap();
    // B must become ready before A can finish. This isolates the readiness
    // wake from the ordinary request-completion wake.
    wait_for_node(&client, &base, |node| {
        node["loaded_models"]
            .as_array()
            .is_some_and(|models| models.iter().any(|model| model == "model-b:default"))
    })
    .await;
    assert!(
        !a.is_finished(),
        "A must still be held behind its response gate"
    );
    std::fs::write(&response_gate, []).unwrap();
    for request in [a, b] {
        let response = request.await.unwrap().unwrap();
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        let body = response.json::<Value>().await.unwrap();
        assert!(
            body["choices"][0]["message"]["content"].is_string(),
            "{body}"
        );
    }
}
