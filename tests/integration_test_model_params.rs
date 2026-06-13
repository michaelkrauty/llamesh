use reqwest::StatusCode;
use std::time::Duration;
use tokio::time::sleep;

mod common;
use common::{
    cleanup_by_port_range_pattern, cleanup_procs, graceful_stop, llamesh_binary, setup_mock_script,
    wait_for_ready,
};

const BASE: &str = "http://127.0.0.1:9205";

async fn fetch_parsed_params_at(base: &str, client: &reqwest::Client) -> Option<serde_json::Value> {
    let resp = client.get(format!("{base}/v1/models")).send().await.ok()?;
    let body: serde_json::Value = resp.json().await.ok()?;
    let entry = body["data"]
        .as_array()?
        .iter()
        .find(|m| m["id"] == "mock-model")?;
    let params = &entry["metadata"]["parsed_model_params"];
    if params.is_null() {
        None
    } else {
        Some(params.clone())
    }
}

async fn fetch_parsed_params(client: &reqwest::Client) -> Option<serde_json::Value> {
    fetch_parsed_params_at(BASE, client).await
}

async fn local_instance_count(client: &reqwest::Client) -> usize {
    let Ok(resp) = client.get(format!("{BASE}/cluster/nodes")).send().await else {
        return usize::MAX;
    };
    let Ok(body) = resp.json::<serde_json::Value>().await else {
        return usize::MAX;
    };
    body["nodes"]["test-node-params"]["active_instances"]
        .as_u64()
        .map(|n| n as usize)
        .unwrap_or(usize::MAX)
}

fn params_node_config(
    suffix: &str,
    listen_port: u16,
    backend_start: u16,
    mock_script: &std::path::Path,
) -> String {
    format!(
        r#"
node_id: "test-node-{suffix}"
listen_addr: "127.0.0.1:{listen_port}"
metrics_path: "./tests/metrics_{suffix}.json"
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
    - start: {backend_start}
      end: {}
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
        backend_start + 9,
        mock_script.display()
    )
}

const PARAMS_COOKBOOK: &str = r#"
models:
  - name: "mock-model"
    profiles:
      - id: "default"
        model_path: "./models/mock.gguf"
        idle_timeout_seconds: 1
        max_instances: 1
        llama_server_args: "-c 4096"
"#;

/// Parsed model params must remain available from /v1/models after the
/// instance that produced them is evicted: they are persisted per args_hash
/// in the metrics store, not tied to the instance lifetime.
#[tokio::test]
async fn test_parsed_params_survive_instance_eviction() {
    cleanup_procs("mock_server_params.sh").await;
    cleanup_procs("config_params.yaml").await;
    cleanup_by_port_range_pattern("1325[0-9]").await;
    let root = std::env::current_dir().unwrap();
    let mock_script = setup_mock_script(&root, "params").await;
    let config_path = root.join("tests/config_params.yaml");
    let cookbook_path = root.join("tests/cookbook_params.yaml");
    let metrics_path = root.join("tests/metrics_params.json");
    let proxy_bin = llamesh_binary(&root);

    // This test asserts on the empty-state behavior, so a metrics snapshot
    // persisted by a previous run must not leak in (the test config points
    // metrics_path at this dedicated file rather than the default).
    let _ = tokio::fs::remove_file(&metrics_path).await;

    let config_content = params_node_config("params", 9205, 13250, &mock_script);
    tokio::fs::write(&config_path, config_content)
        .await
        .unwrap();
    tokio::fs::write(&cookbook_path, PARAMS_COOKBOOK)
        .await
        .unwrap();

    let mut proxy_process = tokio::process::Command::new(&proxy_bin)
        .arg("--config")
        .arg(&config_path)
        .arg("--cookbook")
        .arg(&cookbook_path)
        .kill_on_drop(true)
        .spawn()
        .expect("Failed to start proxy");

    assert!(wait_for_ready(BASE).await, "Proxy failed to become ready");

    let client = reqwest::Client::new();

    // Before any instance has run, there is nothing to report.
    assert!(
        fetch_parsed_params(&client).await.is_none(),
        "expected no parsed params before the first instance start"
    );

    // Spawn an instance by making a request.
    let body = serde_json::json!({
        "model": "mock-model:default",
        "messages": [{"role": "user", "content": "Hello"}],
        "stream": false
    });
    let resp = client
        .post(format!("{BASE}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Params should be visible while (or shortly after) the instance is live.
    let mut params = None;
    for _ in 0..20 {
        params = fetch_parsed_params(&client).await;
        if params.is_some() {
            break;
        }
        sleep(Duration::from_millis(250)).await;
    }
    let params = params.expect("parsed params should appear after instance startup");
    assert_eq!(params["n_ctx"], 4096, "unexpected params: {params}");

    // Wait for the idle instance (1s timeout) to be evicted.
    let mut evicted = false;
    for _ in 0..60 {
        if local_instance_count(&client).await == 0 {
            evicted = true;
            break;
        }
        sleep(Duration::from_millis(500)).await;
    }
    assert!(evicted, "instance was not evicted within the wait window");

    // The instance is gone; the persisted params must still be served.
    let params = fetch_parsed_params(&client)
        .await
        .expect("parsed params should survive instance eviction");
    assert_eq!(
        params["n_ctx"], 4096,
        "persisted params drifted after eviction: {params}"
    );

    graceful_stop(&mut proxy_process).await;
    let _ = tokio::fs::remove_file(&config_path).await;
    let _ = tokio::fs::remove_file(&cookbook_path).await;
    let _ = tokio::fs::remove_file(&mock_script).await;
    let _ = tokio::fs::remove_file(&metrics_path).await;
    let _ = tokio::fs::remove_file(metrics_path.with_extension("tmp")).await;
}

/// Parsed params recorded by an instance must survive a proxy restart that
/// happens *before* the periodic (10s) metrics persistence tick: recording
/// the params triggers an immediate snapshot write. The proxy is restarted
/// as soon as the params are first observable, and must still serve them.
#[tokio::test]
async fn test_parsed_params_survive_immediate_restart() {
    const RBASE: &str = "http://127.0.0.1:9206";
    cleanup_procs("mock_server_params_restart.sh").await;
    cleanup_procs("config_params_restart.yaml").await;
    cleanup_by_port_range_pattern("1326[0-9]").await;
    let root = std::env::current_dir().unwrap();
    let mock_script = setup_mock_script(&root, "params_restart").await;
    let config_path = root.join("tests/config_params_restart.yaml");
    let cookbook_path = root.join("tests/cookbook_params_restart.yaml");
    let metrics_path = root.join("tests/metrics_params_restart.json");
    let proxy_bin = llamesh_binary(&root);

    let _ = tokio::fs::remove_file(&metrics_path).await;

    let config_content = params_node_config("params_restart", 9206, 13260, &mock_script);
    tokio::fs::write(&config_path, config_content)
        .await
        .unwrap();
    tokio::fs::write(&cookbook_path, PARAMS_COOKBOOK)
        .await
        .unwrap();

    let spawn_proxy = || {
        tokio::process::Command::new(&proxy_bin)
            .arg("--config")
            .arg(&config_path)
            .arg("--cookbook")
            .arg(&cookbook_path)
            .kill_on_drop(true)
            .spawn()
            .expect("Failed to start proxy")
    };

    let mut proxy_process = spawn_proxy();
    assert!(wait_for_ready(RBASE).await, "Proxy failed to become ready");

    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "model": "mock-model:default",
        "messages": [{"role": "user", "content": "Hello"}],
        "stream": false
    });
    let resp = client
        .post(format!("{RBASE}/v1/chat/completions"))
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Restart the proxy the moment the params become observable — well
    // within the periodic persistence interval.
    let mut params = None;
    for _ in 0..20 {
        params = fetch_parsed_params_at(RBASE, &client).await;
        if params.is_some() {
            break;
        }
        sleep(Duration::from_millis(250)).await;
    }
    assert!(
        params.is_some(),
        "parsed params should appear after instance startup"
    );
    graceful_stop(&mut proxy_process).await;

    let mut proxy_process = spawn_proxy();
    assert!(
        wait_for_ready(RBASE).await,
        "Proxy failed to become ready after restart"
    );
    let params = fetch_parsed_params_at(RBASE, &client)
        .await
        .expect("parsed params should survive an immediate proxy restart");
    assert_eq!(
        params["n_ctx"], 4096,
        "persisted params drifted across restart: {params}"
    );

    graceful_stop(&mut proxy_process).await;
    let _ = tokio::fs::remove_file(&config_path).await;
    let _ = tokio::fs::remove_file(&cookbook_path).await;
    let _ = tokio::fs::remove_file(&mock_script).await;
    let _ = tokio::fs::remove_file(&metrics_path).await;
    let _ = tokio::fs::remove_file(metrics_path.with_extension("tmp")).await;
}
