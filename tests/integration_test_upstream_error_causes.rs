//! A failed request to a local instance must tell the client why it failed.
//!
//! reqwest's own message for a failed request names only the stage that failed
//! (`error sending request for url (...)`, `request or response body error`);
//! the cause (a timeout, a refused or reset connection) is in its source chain.
//! Both tests bound upstream reads with `upstream_read_timeout_ms` and make the
//! mock server slower than that bound, then require the 502 to name the
//! timeout:
//!
//! * before the response head: the mock delays every non-streaming response
//!   by at least 500 ms, more than the 200 ms bound, so sending the request
//!   fails;
//! * mid-body: `MOCK_SLOW_BODY_MS` makes the mock send the head and half the
//!   body at once, then pause for longer than the 2 s bound, so reading the
//!   body fails. The bound is generous so a slow test host still receives the
//!   head in time.

use reqwest::StatusCode;
use std::time::Duration;

mod common;
use common::{
    cleanup_by_port_range_pattern, cleanup_procs, graceful_stop, llamesh_binary,
    reset_metrics_file, setup_mock_script_with_env, wait_for_ready,
};

const COOKBOOK: &str = r#"
models:
  - name: "mock-model"
    enabled: true
    profiles:
      - id: "default"
        model_path: "./models/mock.gguf"
        idle_timeout_seconds: 60
        max_instances: 1
        llama_server_args: ""
"#;

struct Node {
    suffix: &'static str,
    listen_addr: &'static str,
    port_start: u16,
    port_pattern: &'static str,
    read_timeout_ms: u64,
}

fn config_yaml(node: &Node, mock_script: &std::path::Path) -> String {
    format!(
        r#"
node_id: "test-{suffix}"
listen_addr: "{listen_addr}"
metrics_path: "./tests/metrics_{suffix}.json"
max_vram_mb: 1048576
max_sysmem_mb: 1024
default_model: "mock-model:default"
upstream_read_timeout_ms: {read_timeout_ms}
model_defaults:
  max_concurrent_requests_per_instance: 2
  max_queue_size_per_model: 10
  max_instances_per_model: 1
  max_wait_in_queue_ms: 5000
llama_cpp_ports:
  ranges:
    - start: {port_start}
      end: {port_end}
llama_cpp:
  repo_url: ""
  repo_path: "."
  build_path: "."
  binary_path: "{mock}"
  branch: "master"
  build_args: []
  build_command_args: []
  auto_update_interval_seconds: 0
  enabled: false
cluster:
  enabled: false
  peers: []
  gossip_interval_seconds: 5
http:
  request_body_limit_bytes: 1048576
  idle_timeout_seconds: 60
"#,
        suffix = node.suffix,
        listen_addr = node.listen_addr,
        port_start = node.port_start,
        port_end = node.port_start + 9,
        read_timeout_ms = node.read_timeout_ms,
        mock = mock_script.display()
    )
}

/// Start a node whose mock server runs with `env`, send one non-streaming chat
/// request, and return the response status and error message.
async fn failed_request(node: &Node, env: &[(&str, &str)]) -> (StatusCode, String) {
    let root = std::env::current_dir().unwrap();
    let mock_script = setup_mock_script_with_env(&root, node.suffix, env).await;
    let config_path = root.join(format!("tests/config_{}.yaml", node.suffix));
    let cookbook_path = root.join(format!("tests/cookbook_{}.yaml", node.suffix));
    let metrics_path = reset_metrics_file(&root, node.suffix).await;
    tokio::fs::write(&config_path, config_yaml(node, &mock_script))
        .await
        .unwrap();
    tokio::fs::write(&cookbook_path, COOKBOOK).await.unwrap();

    cleanup_by_port_range_pattern(node.port_pattern).await;
    let mut child = tokio::process::Command::new(llamesh_binary(&root))
        .arg("--config")
        .arg(&config_path)
        .arg("--cookbook")
        .arg(&cookbook_path)
        .kill_on_drop(true)
        .spawn()
        .expect("failed to start llamesh");

    let base_url = format!("http://{}", node.listen_addr);
    let outcome = if wait_for_ready(&base_url).await {
        let response = reqwest::Client::new()
            .post(format!("{base_url}/v1/chat/completions"))
            .json(&serde_json::json!({
                "model": "mock-model:default",
                "messages": [{"role": "user", "content": "hi"}],
            }))
            .timeout(Duration::from_secs(60))
            .send()
            .await;
        match response {
            Ok(response) => {
                let status = response.status();
                let body: serde_json::Value = response.json().await.unwrap_or_default();
                let message = body["error"]["message"].as_str().unwrap_or("").to_string();
                Ok((status, message))
            }
            Err(e) => Err(format!("request failed: {e}")),
        }
    } else {
        Err("llamesh did not become ready".to_string())
    };

    graceful_stop(&mut child).await;
    cleanup_procs(&format!("mock_server_{}.sh", node.suffix)).await;
    cleanup_by_port_range_pattern(node.port_pattern).await;
    for path in [&config_path, &cookbook_path, &mock_script, &metrics_path] {
        let _ = tokio::fs::remove_file(path).await;
    }
    let _ = tokio::fs::remove_file(metrics_path.with_extension("tmp")).await;

    outcome.unwrap_or_else(|e| panic!("{}: {e}", node.suffix))
}

#[tokio::test]
async fn send_failure_names_its_cause() {
    let node = Node {
        suffix: "error_causes_send",
        listen_addr: "127.0.0.1:9251",
        port_start: 13280,
        port_pattern: "1328[0-9]",
        read_timeout_ms: 200,
    };
    let (status, message) = failed_request(&node, &[]).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "message: {message}");
    assert!(
        message.starts_with("Upstream error: error sending request"),
        "unexpected message: {message}"
    );
    assert!(
        message.contains("timed out"),
        "502 does not name the cause: {message}"
    );
}

#[tokio::test]
async fn body_read_failure_names_its_cause() {
    let node = Node {
        suffix: "error_causes_body",
        listen_addr: "127.0.0.1:9252",
        port_start: 13290,
        port_pattern: "1329[0-9]",
        read_timeout_ms: 2000,
    };
    let (status, message) = failed_request(&node, &[("MOCK_SLOW_BODY_MS", "30000")]).await;
    assert_eq!(status, StatusCode::BAD_GATEWAY, "message: {message}");
    assert!(
        message.starts_with("Failed to read upstream response body:"),
        "unexpected message: {message}"
    );
    assert!(
        message.contains("timed out"),
        "502 does not name the cause: {message}"
    );
}
