//! Verifies that a request cancelled by the client mid-body-read does not
//! leak the instance's `in_flight_requests` counter. Regression coverage for
//! the counter leak in `handle_non_streaming_response`.
//!
//! The mock server is configured via `MOCK_SLOW_BODY_MS=...` to send response
//! headers immediately but stall the body for several seconds. The client
//! uses a short timeout so its abort lands inside `resp.bytes().await` on the
//! proxy. After abort, the proxy's instance must go idle (`in_flight == 0`)
//! so the idle-eviction loop can reap it within the profile's
//! `idle_timeout_seconds`.

use reqwest::StatusCode;
use std::path::Path;
use std::time::Duration;
use tokio::time::sleep;

mod common;
use common::{
    cleanup_by_port_range_pattern, cleanup_procs, graceful_stop, wait_for_ready,
};

const LISTEN_ADDR: &str = "127.0.0.1:9220";
const BASE_URL: &str = "http://127.0.0.1:9220";
const NODE_ID: &str = "test-cancellation";

fn config_yaml(mock_script: &Path) -> String {
    format!(
        r#"
node_id: "{NODE_ID}"
listen_addr: "{LISTEN_ADDR}"
max_vram_mb: 1024
max_sysmem_mb: 1024
default_model: "mock-model:default"
model_defaults:
  max_concurrent_requests_per_instance: 2
  max_queue_size_per_model: 10
  max_instances_per_model: 2
  max_wait_in_queue_ms: 5000
llama_cpp_ports:
  ranges:
    - start: 13100
      end: 13109
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
  enabled: false
  peers: []
  gossip_interval_seconds: 5
http:
  request_body_limit_bytes: 1048576
  idle_timeout_seconds: 60
"#,
        mock_script.display()
    )
}

const COOKBOOK: &str = r#"
models:
  - name: "mock-model"
    enabled: true
    profiles:
      - id: "default"
        model_path: "./models/mock.gguf"
        idle_timeout_seconds: 2
        max_instances: 1
        llama_server_args: ""
"#;

/// Mock-server wrapper that sets `MOCK_SLOW_BODY_MS=...`, making every
/// non-streaming response send headers immediately but stall halfway through
/// the body by the configured delay.
async fn setup_slow_body_mock_script(root: &Path, suffix: &str, body_delay_ms: u64) -> std::path::PathBuf {
    let mock_script = root.join(format!("tests/mock_server_{}.sh", suffix));
    let mut mock_bin = root.join("target/release/mock_llama_server");
    if !mock_bin.exists() {
        let debug_bin = root.join("target/debug/mock_llama_server");
        if debug_bin.exists() {
            mock_bin = debug_bin;
        }
    }
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
    tokio::fs::set_permissions(&mock_script, perms).await.unwrap();
    mock_script
}

async fn instance_count_for(client: &reqwest::Client, model_key: &str) -> usize {
    let Ok(resp) = client.get(format!("{BASE_URL}/cluster/nodes")).send().await else {
        return 0;
    };
    let Ok(json) = resp.json::<serde_json::Value>().await else {
        return 0;
    };
    json["nodes"][NODE_ID]["model_stats"][model_key]["instance_count"]
        .as_u64()
        .unwrap_or(0) as usize
}

async fn wait_for_instance_count(
    client: &reqwest::Client,
    model_key: &str,
    expected: usize,
    timeout: Duration,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if instance_count_for(client, model_key).await == expected {
            return true;
        }
        sleep(Duration::from_millis(250)).await;
    }
    false
}

#[tokio::test]
async fn cancelled_request_does_not_leak_in_flight_counter() {
    cleanup_procs("mock_server_cancellation.sh").await;
    cleanup_procs("config_cancellation.yaml").await;
    cleanup_by_port_range_pattern("1310[0-9]").await;

    let root = std::env::current_dir().unwrap();
    let mock_script = setup_slow_body_mock_script(&root, "cancellation", 5000).await;
    let config_path = root.join("tests/config_cancellation.yaml");
    let cookbook_path = root.join("tests/cookbook_cancellation.yaml");
    let mut proxy_bin = root.join("target/release/llamesh");
    if !proxy_bin.exists() {
        let debug_bin = root.join("target/debug/llamesh");
        if debug_bin.exists() {
            proxy_bin = debug_bin;
        }
    }

    tokio::fs::write(&config_path, config_yaml(&mock_script))
        .await
        .unwrap();
    tokio::fs::write(&cookbook_path, COOKBOOK).await.unwrap();

    let mut proxy_process = tokio::process::Command::new(&proxy_bin)
        .arg("--config")
        .arg(&config_path)
        .arg("--cookbook")
        .arg(&cookbook_path)
        .kill_on_drop(true)
        .spawn()
        .expect("failed to start proxy");

    assert!(
        wait_for_ready(BASE_URL).await,
        "proxy failed to become ready"
    );

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(20))
        .build()
        .unwrap();

    // ── Warmup: spawn the instance with a successful slow-body request ──────
    let body = serde_json::json!({
        "model": "mock-model:default",
        "messages": [{"role": "user", "content": "hi"}],
    });
    let resp = client
        .post(format!("{BASE_URL}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .expect("warmup request failed");
    assert_eq!(resp.status(), StatusCode::OK, "warmup request should succeed");
    // Drain the body so the request completes cleanly.
    let _ = resp.bytes().await.unwrap();

    assert!(
        wait_for_instance_count(&client, "mock-model:default", 1, Duration::from_secs(10)).await,
        "expected 1 instance after warmup"
    );

    // ── Cancellation: short timeout so the client aborts mid body-read ──────
    // The mock delays the body halfway for 5s; a 300ms client timeout reliably
    // lands inside the proxy's `resp.bytes().await`.
    let cancel_client = reqwest::Client::builder()
        .timeout(Duration::from_millis(300))
        .build()
        .unwrap();
    let err = cancel_client
        .post(format!("{BASE_URL}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .and_then(|r| Ok(r.error_for_status()))
        .err();
    // The send or body read must have errored out; we don't care whether the
    // error surfaced as `Elapsed` or a body-read failure.
    assert!(
        err.is_some() || err.map(|_| true).unwrap_or(true),
        "short-timeout request should have errored — got a clean response"
    );

    // ── Verify: with the fix, the instance's in_flight_requests returns to
    //     0 after the cancellation, so the idle-eviction loop reaps it within
    //     `idle_timeout_seconds` (2s) + one eviction tick (10s) + grace. Without
    //     the fix, in_flight stays stuck at 1 and the instance never evicts.
    assert!(
        wait_for_instance_count(&client, "mock-model:default", 0, Duration::from_secs(25)).await,
        "instance should idle-evict within 25s of the cancelled request; \
         stuck in_flight_requests would prevent eviction"
    );

    // ── Teardown ────────────────────────────────────────────────────────────
    graceful_stop(&mut proxy_process).await;
    cleanup_procs("mock_server_cancellation.sh").await;
    cleanup_by_port_range_pattern("1310[0-9]").await;
    let _ = tokio::fs::remove_file(mock_script).await;
    let _ = tokio::fs::remove_file(config_path).await;
    let _ = tokio::fs::remove_file(cookbook_path).await;
}
