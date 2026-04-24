//! Verifies that hot-reloading the cookbook drains running instances whose
//! (model, profile, args_hash) no longer matches the cookbook. Regression
//! coverage for <https://github.com/michaelkrauty/llamesh/issues/7>.

use reqwest::StatusCode;
use std::time::Duration;
use tokio::time::sleep;

mod common;
use common::{
    cleanup_by_port_range_pattern, cleanup_procs, graceful_stop, llamesh_binary, setup_mock_script,
    wait_for_ready,
};

const LISTEN_ADDR: &str = "127.0.0.1:9210";
const BASE_URL: &str = "http://127.0.0.1:9210";

fn config_yaml(mock_script: &std::path::Path) -> String {
    format!(
        r#"
node_id: "test-reload-drain"
listen_addr: "{}"
max_vram_mb: 1048576
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

const COOKBOOK_PROFILES_ENABLED: &str = r#"
models:
  - name: "mock-model"
    enabled: true
    profiles:
      - id: "default"
        model_path: "./models/mock.gguf"
        idle_timeout_seconds: 600
        max_instances: 1
        llama_server_args: ""
      - id: "alt"
        model_path: "./models/mock-alt.gguf"
        idle_timeout_seconds: 600
        max_instances: 1
        llama_server_args: "--alias mock-alt"
  - name: "other-model"
    enabled: true
    profiles:
      - id: "default"
        model_path: "./models/other.gguf"
        idle_timeout_seconds: 600
        max_instances: 1
        llama_server_args: ""
"#;

const COOKBOOK_ALT_PROFILE_DISABLED: &str = r#"
models:
  - name: "mock-model"
    enabled: true
    profiles:
      - id: "default"
        model_path: "./models/mock.gguf"
        idle_timeout_seconds: 600
        max_instances: 1
        llama_server_args: ""
      - id: "alt"
        enabled: false
        model_path: "./models/mock-alt.gguf"
        idle_timeout_seconds: 600
        max_instances: 1
        llama_server_args: "--alias mock-alt"
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
        if let Ok(resp) = client.get(format!("{BASE_URL}/cluster/nodes")).send().await {
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

/// Poll `/v1/models` until `model_id` is absent from the listed models, or the
/// deadline elapses. Used to synchronize with cookbook hot-reload.
async fn wait_for_model_unlisted(
    client: &reqwest::Client,
    model_id: &str,
    timeout: Duration,
) -> bool {
    let deadline = std::time::Instant::now() + timeout;
    while std::time::Instant::now() < deadline {
        if let Ok(resp) = client.get(format!("{BASE_URL}/v1/models")).send().await {
            if let Ok(json) = resp.json::<serde_json::Value>().await {
                let data = json["data"].as_array().cloned().unwrap_or_default();
                if !data.iter().any(|m| m["id"].as_str() == Some(model_id)) {
                    return true;
                }
            }
        }
        sleep(Duration::from_millis(100)).await;
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
        if let Ok(resp) = client.get(format!("{BASE_URL}/v1/models")).send().await {
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
        .post(format!("{BASE_URL}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .expect("request failed");
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "request to spawn {model} failed"
    );
}

/// In-place overwrite. Preserves the inode; triggers `IN_MODIFY`.
async fn write_cookbook_inplace(path: &std::path::Path, content: &str) {
    tokio::fs::write(path, content).await.unwrap();
}

/// Rename-based write. Writes a sibling temp file, then atomically renames
/// it over `path`. This is the pattern used by `sed -i`, `vim` with its
/// default `:set writebackup`, and most IDEs' save-atomic. The inode of the
/// file at `path` changes — which used to silently break the watcher when
/// it was bound to the file inode rather than the parent directory.
async fn write_cookbook_rename(path: &std::path::Path, content: &str) {
    let tmp = path.with_extension("yaml.tmp");
    tokio::fs::write(&tmp, content).await.unwrap();
    tokio::fs::rename(&tmp, path).await.unwrap();
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
    let proxy_bin = llamesh_binary(&root);

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
        .get(format!("{BASE_URL}/cluster/nodes"))
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
        "expected mock-model:default to be loaded, got {loaded:?}"
    );

    // Case 1 uses an in-place write (the OpenAI-text-editor path).
    write_cookbook_inplace(&cookbook_path, COOKBOOK_DISABLED).await;

    assert!(
        wait_for_unload(&client, "mock-model:default", Duration::from_secs(30)).await,
        "mock-model:default should have been drained within 30s after disable"
    );

    // ── Case 2: args_hash change via atomic rename-over drains the stale
    //     instance. The rename-over path is how `sed -i` and most editors
    //     save files, and historically it broke the watcher after the first
    //     rename (inotify's watch was bound to the replaced inode). The fix
    //     watches the parent directory and filters by filename, so it keeps
    //     receiving events after any number of rename-over edits. ──────────
    write_cookbook_rename(&cookbook_path, COOKBOOK_ENABLED).await;
    assert!(
        wait_for_model_listed(&client, "mock-model", Duration::from_secs(10)).await,
        "cookbook re-enable should make mock-model listed within 10s"
    );

    spawn_instance_for(&client, "mock-model:default").await;

    write_cookbook_rename(&cookbook_path, COOKBOOK_ARGS_CHANGED).await;

    assert!(
        wait_for_unload(&client, "mock-model:default", Duration::from_secs(30)).await,
        "mock-model:default should have been drained within 30s after args_hash change \
         (regression guard: rename-over must not silence the watcher)"
    );

    // ── Case 3: disabling one profile drains only that profile while sibling
    //     profiles stay routable. ─────────────────────────────────────────────
    write_cookbook_rename(&cookbook_path, COOKBOOK_PROFILES_ENABLED).await;
    assert!(
        wait_for_model_listed(&client, "mock-model:alt", Duration::from_secs(10)).await,
        "cookbook reload should make mock-model:alt listed within 10s"
    );

    spawn_instance_for(&client, "mock-model:alt").await;

    write_cookbook_inplace(&cookbook_path, COOKBOOK_ALT_PROFILE_DISABLED).await;

    assert!(
        wait_for_model_unlisted(&client, "mock-model:alt", Duration::from_secs(10)).await,
        "disabled profile should disappear from /v1/models within 10s"
    );
    assert!(
        wait_for_unload(&client, "mock-model:alt", Duration::from_secs(30)).await,
        "mock-model:alt should have been drained within 30s after profile disable"
    );

    spawn_instance_for(&client, "mock-model:default").await;

    // ── Teardown ────────────────────────────────────────────────────────────
    graceful_stop(&mut proxy_process).await;
    cleanup_procs("mock_server_reload_eviction.sh").await;
    cleanup_by_port_range_pattern("1309[0-9]").await;
    let _ = tokio::fs::remove_file(mock_script).await;
    let _ = tokio::fs::remove_file(config_path).await;
    let _ = tokio::fs::remove_file(cookbook_path).await;
}
