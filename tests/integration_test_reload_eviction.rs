//! Verifies that hot-reloading the cookbook drains running instances whose
//! (model, profile, args_hash) no longer matches the cookbook. Regression
//! coverage for <https://github.com/michaelkrauty/llamesh/issues/7>.

use reqwest::StatusCode;
use std::time::Duration;
use tokio::time::sleep;

mod common;
use common::{
    cleanup_by_port_range_pattern, cleanup_procs, graceful_stop, setup_mock_script, wait_for_ready,
};

const LISTEN_ADDR: &str = "127.0.0.1:9210";
const BASE_URL: &str = "http://127.0.0.1:9210";

fn config_yaml(mock_script: &std::path::Path) -> String {
    format!(
        r#"
node_id: "test-reload-drain"
listen_addr: "{}"
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
    - start: 13090
      end: 13099
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
        LISTEN_ADDR,
        mock_script.display()
    )
}

const COOKBOOK_ENABLED: &str = r#"
models:
  - name: "mock-model"
    enabled: true
    profiles:
      - id: "default"
        model_path: "./models/mock.gguf"
        idle_timeout_seconds: 600
        max_instances: 1
        llama_server_args: ""
  - name: "other-model"
    enabled: true
    profiles:
      - id: "default"
        model_path: "./models/other.gguf"
        idle_timeout_seconds: 600
        max_instances: 1
        llama_server_args: ""
"#;

const COOKBOOK_DISABLED: &str = r#"
models:
  - name: "mock-model"
    enabled: false
    profiles:
      - id: "default"
        model_path: "./models/mock.gguf"
        idle_timeout_seconds: 600
        max_instances: 1
        llama_server_args: ""
  - name: "other-model"
    enabled: true
    profiles:
      - id: "default"
        model_path: "./models/other.gguf"
        idle_timeout_seconds: 600
        max_instances: 1
        llama_server_args: ""
"#;

const COOKBOOK_ARGS_CHANGED: &str = r#"
models:
  - name: "mock-model"
    enabled: true
    profiles:
      - id: "default"
        model_path: "./models/mock.gguf"
        idle_timeout_seconds: 600
        max_instances: 1
        llama_server_args: "--ctx-size 4096"
  - name: "other-model"
    enabled: true
    profiles:
      - id: "default"
        model_path: "./models/other.gguf"
        idle_timeout_seconds: 600
        max_instances: 1
        llama_server_args: ""
"#;

/// Poll `/cluster/nodes` until `mock-model:default` is no longer present in the
/// local node's `loaded_models`, or the deadline elapses.
async fn wait_for_unload(client: &reqwest::Client, model_key: &str, timeout: Duration) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if let Ok(resp) = client.get(format!("{}/cluster/nodes", BASE_URL)).send().await {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                let loaded = json["nodes"]["test-reload-drain"]["loaded_models"]
                    .as_array()
                    .cloned()
                    .unwrap_or_default();
                if !loaded.iter().any(|m| m.as_str() == Some(model_key)) {
                    return true;
                }
            }
        }
        sleep(Duration::from_millis(250)).await;
    }
    false
}

/// Poll `/v1/models` until `model_id` appears in the listed models, or the
/// deadline elapses. Used to synchronize with cookbook hot-reload.
async fn wait_for_model_listed(
    client: &reqwest::Client,
    model_id: &str,
    timeout: Duration,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if let Ok(resp) = client.get(format!("{}/v1/models", BASE_URL)).send().await {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                let data = json["data"].as_array().cloned().unwrap_or_default();
                if data.iter().any(|m| m["id"].as_str() == Some(model_id)) {
                    return true;
                }
            }
        }
        sleep(Duration::from_millis(100)).await;
    }
    false
}

async fn spawn_instance_for(client: &reqwest::Client, model: &str) {
    let body = serde_json::json!({
        "model": model,
        "messages": [{"role": "user", "content": "hi"}],
    });
    let resp = client
        .post(format!("{}/v1/chat/completions", BASE_URL))
        .json(&body)
        .send()
        .await
        .expect("request failed");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "request to spawn {} failed",
        model
    );
}

async fn write_cookbook(path: &std::path::Path, content: &str) {
    // Write in place. A rename-over would break the inotify watcher on Linux:
    // the watch is set on the inode, and renaming a new file over the target
    // replaces the inode without firing events on the new one.
    tokio::fs::write(path, content).await.unwrap();
}

#[tokio::test]
async fn reload_drains_orphaned_instances() {
    cleanup_procs("mock_server_reload_eviction.sh").await;
    cleanup_procs("config_reload_eviction.yaml").await;
    cleanup_by_port_range_pattern("1309[0-9]").await;

    let root = std::env::current_dir().unwrap();
    let mock_script = setup_mock_script(&root, "reload_eviction").await;
    let config_path = root.join("tests/config_reload_eviction.yaml");
    let cookbook_path = root.join("tests/cookbook_reload_eviction.yaml");
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
    tokio::fs::write(&cookbook_path, COOKBOOK_ENABLED)
        .await
        .unwrap();

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
        .timeout(Duration::from_secs(15))
        .build()
        .unwrap();

    // ── Case 1: disabling a model drains its running instance ───────────────
    spawn_instance_for(&client, "mock-model:default").await;

    let resp = client
        .get(format!("{}/cluster/nodes", BASE_URL))
        .send()
        .await
        .unwrap();
    let json: serde_json::Value = resp.json().await.unwrap();
    let loaded = json["nodes"]["test-reload-drain"]["loaded_models"]
        .as_array()
        .cloned()
        .unwrap_or_default();
    assert!(
        loaded
            .iter()
            .any(|m| m.as_str() == Some("mock-model:default")),
        "expected mock-model:default to be loaded, got {:?}",
        loaded
    );

    write_cookbook(&cookbook_path, COOKBOOK_DISABLED).await;

    assert!(
        wait_for_unload(&client, "mock-model:default", Duration::from_secs(30)).await,
        "mock-model:default should have been drained within 30s after disable"
    );

    // ── Case 2: changing llama_server_args drains the stale instance ────────
    write_cookbook(&cookbook_path, COOKBOOK_ENABLED).await;
    // /v1/models exposes default-profile models under the bare model name.
    assert!(
        wait_for_model_listed(&client, "mock-model", Duration::from_secs(10)).await,
        "cookbook re-enable should make mock-model listed within 10s"
    );

    spawn_instance_for(&client, "mock-model:default").await;

    write_cookbook(&cookbook_path, COOKBOOK_ARGS_CHANGED).await;

    assert!(
        wait_for_unload(&client, "mock-model:default", Duration::from_secs(30)).await,
        "mock-model:default should have been drained within 30s after args_hash change"
    );

    // ── Teardown ────────────────────────────────────────────────────────────
    graceful_stop(&mut proxy_process).await;
    cleanup_procs("mock_server_reload_eviction.sh").await;
    cleanup_by_port_range_pattern("1309[0-9]").await;
    let _ = tokio::fs::remove_file(mock_script).await;
    let _ = tokio::fs::remove_file(config_path).await;
    let _ = tokio::fs::remove_file(cookbook_path).await;
}
