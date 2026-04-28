//! Verifies that cancelled requests in the forward-to-peer path do not leak
//! the node-level `current_requests` gauge. Regression coverage for two peer
//! forwarding cancellation sites:
//!
//! - forwarding before response headers arrive, where a missing `RequestGuard`
//!   let the `inc_requests()` at the top of the path go unbalanced;
//! - Noise peer response body buffering after response headers arrive, where
//!   the cleanup future was not guarded before awaiting the body stream.
//!
//! Setup: two nodes A and B with gossip enabled. Node A owns `peer-model`,
//! Node B does not. A client sends a request for `peer-model` to Node B with
//! a short timeout; Node B forwards to Node A, but the mock llama-server on
//! Node A takes 500-2000ms to produce headers, so the client's 300ms timeout
//! aborts while Node B is still inside `client_req.send().await`.
//!
//! Assertion: Node B's `current_requests` returns to 0 within a bounded
//! interval after the abort. Without the fix, it stays > 0 forever.

use reqwest::StatusCode;
use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::time::sleep;

mod common;
use common::{
    cleanup_by_port_range_pattern, cleanup_procs, graceful_stop, llamesh_binary, setup_mock_script,
    wait_for_ready,
};

const NODE_A_ADDR: &str = "127.0.0.1:9230";
const NODE_B_ADDR: &str = "127.0.0.1:9231";
const NODE_A_URL: &str = "http://127.0.0.1:9230";
const NODE_B_URL: &str = "http://127.0.0.1:9231";
const NODE_A_BODY_ADDR: &str = "127.0.0.1:9232";
const NODE_B_BODY_ADDR: &str = "127.0.0.1:9233";
const NODE_A_BODY_URL: &str = "http://127.0.0.1:9232";
const NODE_B_BODY_URL: &str = "http://127.0.0.1:9233";

fn config_node(
    node_id: &str,
    listen: &str,
    peer: &str,
    port_start: u16,
    port_end: u16,
    mock_script: &Path,
) -> String {
    format!(
        r#"
node_id: "{node_id}"
listen_addr: "{listen}"
public_url: "http://{listen}"
max_vram_mb: 1048576
max_sysmem_mb: 1024
default_model: "peer-model:default"
model_defaults:
  max_concurrent_requests_per_instance: 2
  max_queue_size_per_model: 10
  max_instances_per_model: 2
  max_wait_in_queue_ms: 5000
llama_cpp_ports:
  ranges:
    - start: {port_start}
      end: {port_end}
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
  peers: ["{peer}"]
  gossip_interval_seconds: 1
http:
  request_body_limit_bytes: 1048576
  idle_timeout_seconds: 60
"#,
        mock_script.display(),
    )
}

const COOKBOOK_A: &str = r#"
models:
  - name: "peer-model"
    enabled: true
    profiles:
      - id: "default"
        model_path: "./models/mock.gguf"
        idle_timeout_seconds: 60
        max_instances: 1
        llama_server_args: ""
"#;

// Node B does not know about peer-model; B must forward to A.
const COOKBOOK_B: &str = r#"
models:
  - name: "other-model"
    enabled: true
    profiles:
      - id: "default"
        model_path: "./models/other.gguf"
        idle_timeout_seconds: 60
        max_instances: 1
        llama_server_args: ""
"#;

/// Mock-server wrapper that sends response headers promptly, then pauses
/// halfway through the non-streaming body.
async fn setup_slow_body_mock_script(root: &Path, suffix: &str, body_delay_ms: u64) -> PathBuf {
    let mock_script = root.join(format!("tests/mock_server_{suffix}.sh"));
    let mock_bin = std::env::var_os("CARGO_BIN_EXE_mock_llama_server")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let debug_bin = root.join("target/debug/mock_llama_server");
            if debug_bin.exists() {
                debug_bin
            } else {
                root.join("target/release/mock_llama_server")
            }
        });
    let script = format!(
        r#"#!/bin/bash
export MOCK_SLOW_BODY_MS={body_delay_ms}
BIN="{}"
"$BIN" "$@" &
PID=$!
trap "kill $PID" EXIT TERM INT
wait $PID
"#,
        mock_bin.display()
    );
    tokio::fs::write(&mock_script, script).await.unwrap();
    use std::os::unix::fs::PermissionsExt;
    let mut perms = tokio::fs::metadata(&mock_script)
        .await
        .unwrap()
        .permissions();
    perms.set_mode(0o755);
    tokio::fs::set_permissions(&mock_script, perms)
        .await
        .unwrap();
    mock_script
}

async fn current_requests(client: &reqwest::Client, base_url: &str) -> u64 {
    let Ok(resp) = client.get(format!("{base_url}/metrics/json")).send().await else {
        return u64::MAX;
    };
    let Ok(json) = resp.json::<serde_json::Value>().await else {
        return u64::MAX;
    };
    json["current_requests"].as_u64().unwrap_or(u64::MAX)
}

async fn wait_for_current_requests(
    client: &reqwest::Client,
    base_url: &str,
    expected: u64,
    timeout: Duration,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if current_requests(client, base_url).await == expected {
            return true;
        }
        sleep(Duration::from_millis(100)).await;
    }
    false
}

async fn wait_for_peer_discovery(
    client: &reqwest::Client,
    base_url: &str,
    peer_node_id: &str,
    timeout: Duration,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if let Ok(resp) = client.get(format!("{base_url}/cluster/nodes")).send().await {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                if json["nodes"][peer_node_id]["ready"]
                    .as_bool()
                    .unwrap_or(false)
                {
                    return true;
                }
            }
        }
        sleep(Duration::from_millis(200)).await;
    }
    false
}

#[tokio::test]
async fn cancelled_peer_forward_does_not_leak_current_requests() {
    cleanup_procs("mock_server_peer_fwd_cancel.sh").await;
    cleanup_procs("config_peer_fwd_cancel").await;
    cleanup_by_port_range_pattern("1312[0-9]").await;

    let root = std::env::current_dir().unwrap();
    let mock_script = setup_mock_script(&root, "peer_fwd_cancel").await;

    let config_a_path = root.join("tests/config_peer_fwd_cancel_a.yaml");
    let cookbook_a_path = root.join("tests/cookbook_peer_fwd_cancel_a.yaml");
    let config_b_path = root.join("tests/config_peer_fwd_cancel_b.yaml");
    let cookbook_b_path = root.join("tests/cookbook_peer_fwd_cancel_b.yaml");

    let proxy_bin = llamesh_binary(&root);

    tokio::fs::write(
        &config_a_path,
        config_node(
            "node-a-cancel",
            NODE_A_ADDR,
            NODE_B_URL,
            13120,
            13124,
            &mock_script,
        ),
    )
    .await
    .unwrap();
    tokio::fs::write(&cookbook_a_path, COOKBOOK_A)
        .await
        .unwrap();
    tokio::fs::write(
        &config_b_path,
        config_node(
            "node-b-cancel",
            NODE_B_ADDR,
            NODE_A_URL,
            13125,
            13129,
            &mock_script,
        ),
    )
    .await
    .unwrap();
    tokio::fs::write(&cookbook_b_path, COOKBOOK_B)
        .await
        .unwrap();

    let mut proxy_a = tokio::process::Command::new(&proxy_bin)
        .arg("--config")
        .arg(&config_a_path)
        .arg("--cookbook")
        .arg(&cookbook_a_path)
        .kill_on_drop(true)
        .spawn()
        .expect("failed to start proxy A");
    let mut proxy_b = tokio::process::Command::new(&proxy_bin)
        .arg("--config")
        .arg(&config_b_path)
        .arg("--cookbook")
        .arg(&cookbook_b_path)
        .kill_on_drop(true)
        .spawn()
        .expect("failed to start proxy B");

    assert!(wait_for_ready(NODE_A_URL).await, "node A failed to ready");
    assert!(wait_for_ready(NODE_B_URL).await, "node B failed to ready");

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();

    // Wait for gossip to propagate A's identity to B.
    assert!(
        wait_for_peer_discovery(
            &client,
            NODE_B_URL,
            "node-a-cancel",
            Duration::from_secs(15)
        )
        .await,
        "node B should discover node A within 15s"
    );

    assert_eq!(
        current_requests(&client, NODE_B_URL).await,
        0,
        "node B should have 0 in-flight requests before the test"
    );

    // Short-timeout client abort while B is inside `client_req.send().await`
    // forwarding to A. The mock's default random 500-2000ms header delay is
    // long enough to land cancellation in the send phase with a 300ms timeout.
    let cancel_client = reqwest::Client::builder()
        .timeout(Duration::from_millis(300))
        .build()
        .unwrap();
    let body = serde_json::json!({
        "model": "peer-model:default",
        "messages": [{"role": "user", "content": "hi"}],
    });
    let _ = cancel_client
        .post(format!("{NODE_B_URL}/v1/chat/completions"))
        .json(&body)
        .send()
        .await;
    // Don't assert on the result — the request is expected to fail (timeout).

    // With the fix: RequestGuard::Drop decrements current_requests when the
    // B-side handler future is dropped. Poll briefly to absorb any remaining
    // inflight work from background tasks.
    assert!(
        wait_for_current_requests(&client, NODE_B_URL, 0, Duration::from_secs(10)).await,
        "node B's current_requests should return to 0 within 10s after cancel; \
         observed {}",
        current_requests(&client, NODE_B_URL).await
    );
    assert!(
        wait_for_current_requests(&client, NODE_A_URL, 0, Duration::from_secs(10)).await,
        "node A should finish any peer work that was already accepted before the sanity request"
    );

    // Sanity: a regular (non-cancelled) forward-to-peer request still completes.
    let ok_client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();
    let resp = ok_client
        .post(format!("{NODE_B_URL}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .expect("non-cancelled forward should succeed");
    assert_eq!(resp.status(), StatusCode::OK);
    let _ = resp.bytes().await;

    assert!(
        wait_for_current_requests(&client, NODE_B_URL, 0, Duration::from_secs(5)).await,
        "node B's current_requests should return to 0 after a successful forward"
    );

    graceful_stop(&mut proxy_a).await;
    graceful_stop(&mut proxy_b).await;
    cleanup_procs("mock_server_peer_fwd_cancel.sh").await;
    cleanup_by_port_range_pattern("1312[0-9]").await;
    let _ = tokio::fs::remove_file(mock_script).await;
    let _ = tokio::fs::remove_file(config_a_path).await;
    let _ = tokio::fs::remove_file(cookbook_a_path).await;
    let _ = tokio::fs::remove_file(config_b_path).await;
    let _ = tokio::fs::remove_file(cookbook_b_path).await;
}

#[tokio::test]
async fn cancelled_peer_forward_during_noise_body_buffer_does_not_leak_current_requests() {
    cleanup_procs("mock_server_peer_fwd_body_cancel.sh").await;
    cleanup_procs("config_peer_fwd_body_cancel").await;
    cleanup_by_port_range_pattern("1313[0-9]").await;

    let root = std::env::current_dir().unwrap();
    let mock_script = setup_slow_body_mock_script(&root, "peer_fwd_body_cancel", 5000).await;

    let config_a_path = root.join("tests/config_peer_fwd_body_cancel_a.yaml");
    let cookbook_a_path = root.join("tests/cookbook_peer_fwd_body_cancel_a.yaml");
    let config_b_path = root.join("tests/config_peer_fwd_body_cancel_b.yaml");
    let cookbook_b_path = root.join("tests/cookbook_peer_fwd_body_cancel_b.yaml");

    let proxy_bin = llamesh_binary(&root);

    tokio::fs::write(
        &config_a_path,
        config_node(
            "node-a-body-cancel",
            NODE_A_BODY_ADDR,
            NODE_B_BODY_URL,
            13130,
            13134,
            &mock_script,
        ),
    )
    .await
    .unwrap();
    tokio::fs::write(&cookbook_a_path, COOKBOOK_A)
        .await
        .unwrap();
    tokio::fs::write(
        &config_b_path,
        config_node(
            "node-b-body-cancel",
            NODE_B_BODY_ADDR,
            NODE_A_BODY_URL,
            13135,
            13139,
            &mock_script,
        ),
    )
    .await
    .unwrap();
    tokio::fs::write(&cookbook_b_path, COOKBOOK_B)
        .await
        .unwrap();

    let mut proxy_a = tokio::process::Command::new(&proxy_bin)
        .arg("--config")
        .arg(&config_a_path)
        .arg("--cookbook")
        .arg(&cookbook_a_path)
        .kill_on_drop(true)
        .spawn()
        .expect("failed to start proxy A");
    let mut proxy_b = tokio::process::Command::new(&proxy_bin)
        .arg("--config")
        .arg(&config_b_path)
        .arg("--cookbook")
        .arg(&cookbook_b_path)
        .kill_on_drop(true)
        .spawn()
        .expect("failed to start proxy B");

    assert!(
        wait_for_ready(NODE_A_BODY_URL).await,
        "node A failed to ready"
    );
    assert!(
        wait_for_ready(NODE_B_BODY_URL).await,
        "node B failed to ready"
    );

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(10))
        .build()
        .unwrap();

    assert!(
        wait_for_peer_discovery(
            &client,
            NODE_B_BODY_URL,
            "node-a-body-cancel",
            Duration::from_secs(15)
        )
        .await,
        "node B should discover node A within 15s"
    );

    let body = serde_json::json!({
        "model": "peer-model:default",
        "messages": [{"role": "user", "content": "hi"}],
    });

    // Warm up the peer path and spawn A's instance so the cancellation below
    // lands after peer response headers, inside B's Noise body buffering.
    let warmup_resp = client
        .post(format!("{NODE_B_BODY_URL}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .expect("warmup forward should succeed");
    assert_eq!(warmup_resp.status(), StatusCode::OK);
    let _ = warmup_resp.bytes().await.expect("warmup body should read");

    assert!(
        wait_for_current_requests(&client, NODE_B_BODY_URL, 0, Duration::from_secs(10)).await,
        "node B should have 0 in-flight requests after warmup"
    );
    assert!(
        wait_for_current_requests(&client, NODE_A_BODY_URL, 0, Duration::from_secs(10)).await,
        "node A should have 0 in-flight requests after warmup"
    );

    // The mock sends peer response headers promptly, then stalls the
    // non-streaming body for 5s. A 1200ms client timeout cancels B while it is
    // awaiting the peer Noise body stream.
    let cancel_client = reqwest::Client::builder()
        .timeout(Duration::from_millis(1200))
        .build()
        .unwrap();
    let result = cancel_client
        .post(format!("{NODE_B_BODY_URL}/v1/chat/completions"))
        .json(&body)
        .send()
        .await;
    assert!(
        result.is_err(),
        "short-timeout request should fail before B finishes buffering peer body"
    );

    assert!(
        wait_for_current_requests(&client, NODE_B_BODY_URL, 0, Duration::from_secs(10)).await,
        "node B's current_requests should return to 0 within 10s after body-buffer cancel; \
         observed {}",
        current_requests(&client, NODE_B_BODY_URL).await
    );
    assert!(
        wait_for_current_requests(&client, NODE_A_BODY_URL, 0, Duration::from_secs(10)).await,
        "node A should finish any peer work accepted before B's client cancelled"
    );

    graceful_stop(&mut proxy_a).await;
    graceful_stop(&mut proxy_b).await;
    cleanup_procs("mock_server_peer_fwd_body_cancel.sh").await;
    cleanup_by_port_range_pattern("1313[0-9]").await;
    let _ = tokio::fs::remove_file(mock_script).await;
    let _ = tokio::fs::remove_file(config_a_path).await;
    let _ = tokio::fs::remove_file(cookbook_a_path).await;
    let _ = tokio::fs::remove_file(config_b_path).await;
    let _ = tokio::fs::remove_file(cookbook_b_path).await;
}
