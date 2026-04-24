use reqwest::StatusCode;
use std::time::Duration;
use tokio::time::sleep;

mod common;
use common::{
    cleanup_by_port_range_pattern, cleanup_procs, graceful_stop, setup_mock_script, wait_for_ready,
};

#[tokio::test]
async fn test_standalone_proxy() {
    cleanup_procs("mock_server_standalone.sh").await;
    cleanup_procs("config_standalone.yaml").await;
    cleanup_by_port_range_pattern("1300[0-9]").await;
    let root = std::env::current_dir().unwrap();
    let mock_script = setup_mock_script(&root, "standalone").await;
    let config_path = root.join("tests/config_standalone.yaml");
    let cookbook_path = root.join("tests/cookbook_standalone.yaml");
    let mut proxy_bin = root.join("target/release/llamesh");
    if !proxy_bin.exists() {
        let debug_bin = root.join("target/debug/llamesh");
        if debug_bin.exists() {
            proxy_bin = debug_bin;
        }
    }

    let config_content = format!(
        r#"
node_id: "test-node"
listen_addr: "127.0.0.1:9190"
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
    - start: 13000
      end: 13009
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
    );

    tokio::fs::write(&config_path, config_content)
        .await
        .unwrap();

    let cookbook_content = r#"
models:
  - name: "mock-model"
    description: "Mock model for testing"
    profiles:
      - id: "default"
        model_path: "./models/mock.gguf"
        idle_timeout_seconds: 5
        max_instances: 1
        llama_server_args: ""
"#;
    tokio::fs::write(&cookbook_path, cookbook_content)
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

    assert!(
        wait_for_ready("http://127.0.0.1:9190").await,
        "Proxy failed to become ready"
    );

    let client = reqwest::Client::new();
    let resp = client
        .get("http://127.0.0.1:9190/v1/models")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    let body = serde_json::json!({
        "model": "mock-model:default",
        "messages": [{"role": "user", "content": "Hello"}],
        "stream": false
    });
    let resp = client
        .post("http://127.0.0.1:9190/v1/chat/completions")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    graceful_stop(&mut proxy_process).await;
    cleanup_procs("mock_server_standalone.sh").await;
    let _ = tokio::fs::remove_file(mock_script).await;
    let _ = tokio::fs::remove_file(config_path).await;
    let _ = tokio::fs::remove_file(cookbook_path).await;
}

#[tokio::test]
async fn test_cluster_proxy() {
    cleanup_procs("mock_server_cluster.sh").await;
    cleanup_procs("config_node_a.yaml").await;
    cleanup_procs("config_node_b.yaml").await;
    cleanup_by_port_range_pattern("130[12][0-9]").await;
    let root = std::env::current_dir().unwrap();
    let mock_script = setup_mock_script(&root, "cluster").await;
    let mut proxy_bin = root.join("target/release/llamesh");
    if !proxy_bin.exists() {
        let debug_bin = root.join("target/debug/llamesh");
        if debug_bin.exists() {
            proxy_bin = debug_bin;
        }
    }

    let config_a_path = root.join("tests/config_node_a.yaml");
    let cookbook_a_path = root.join("tests/cookbook_node_a.yaml");
    let config_b_path = root.join("tests/config_node_b.yaml");
    let cookbook_b_path = root.join("tests/cookbook_node_b.yaml");

    // Node A (9192) knows about Node B (9193)
    let config_a_content = format!(
        r#"
node_id: "node-a"
listen_addr: "127.0.0.1:9192"
public_url: "http://127.0.0.1:9192"
max_vram_mb: 1048576
max_sysmem_mb: 1024
default_model: "model-a:default"
model_defaults:
  max_concurrent_requests_per_instance: 2
  max_queue_size_per_model: 10
  max_instances_per_model: 2
  max_wait_in_queue_ms: 5000
llama_cpp_ports:
  ranges:
    - start: 13010
      end: 13019
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
  peers: ["http://127.0.0.1:9193"]
  gossip_interval_seconds: 2
http:
  request_body_limit_bytes: 1048576
  idle_timeout_seconds: 60
"#,
        mock_script.display()
    );
    tokio::fs::write(&config_a_path, config_a_content)
        .await
        .unwrap();

    let cookbook_a_content = r#"
models:
  - name: "model-a"
    profiles:
      - id: "default"
        model_path: "./models/mock-a.gguf"
        idle_timeout_seconds: 5
        max_instances: 1
        llama_server_args: ""
  - name: "model-b"
    profiles:
      - id: "default"
        model_path: "./models/mock-b.gguf"
        idle_timeout_seconds: 5
        max_instances: 0 # Do not run locally
        llama_server_args: ""
"#;
    tokio::fs::write(&cookbook_a_path, cookbook_a_content)
        .await
        .unwrap();

    // Node B (9193) knows about Node A (9192)
    let config_b_content = format!(
        r#"
node_id: "node-b"
listen_addr: "127.0.0.1:9193"
public_url: "http://127.0.0.1:9193"
max_vram_mb: 1048576
max_sysmem_mb: 1024
default_model: "model-b:default"
model_defaults:
  max_concurrent_requests_per_instance: 2
  max_queue_size_per_model: 10
  max_instances_per_model: 2
  max_wait_in_queue_ms: 5000
llama_cpp_ports:
  ranges:
    - start: 13020
      end: 13029
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
  peers: ["http://127.0.0.1:9192"]
  gossip_interval_seconds: 2
http:
  request_body_limit_bytes: 1048576
  idle_timeout_seconds: 60
"#,
        mock_script.display()
    );
    tokio::fs::write(&config_b_path, config_b_content)
        .await
        .unwrap();

    let cookbook_b_content = r#"
models:
  - name: "model-b"
    profiles:
      - id: "default"
        model_path: "./models/mock-b.gguf"
        idle_timeout_seconds: 5
        max_instances: 1
        llama_server_args: ""
"#;
    tokio::fs::write(&cookbook_b_path, cookbook_b_content)
        .await
        .unwrap();

    // Start Node A
    let mut process_a = tokio::process::Command::new(&proxy_bin)
        .arg("--config")
        .arg(&config_a_path)
        .arg("--cookbook")
        .arg(&cookbook_a_path)
        .kill_on_drop(true)
        .spawn()
        .expect("Failed to start proxy A");

    // Start Node B
    let mut process_b = tokio::process::Command::new(&proxy_bin)
        .arg("--config")
        .arg(&config_b_path)
        .arg("--cookbook")
        .arg(&cookbook_b_path)
        .kill_on_drop(true)
        .spawn()
        .expect("Failed to start proxy B");

    assert!(
        wait_for_ready("http://127.0.0.1:9192").await,
        "Node A failed to ready"
    );
    assert!(
        wait_for_ready("http://127.0.0.1:9193").await,
        "Node B failed to ready"
    );

    // Wait for gossip exchange (a few seconds)
    sleep(Duration::from_secs(5)).await;

    let client = reqwest::Client::new();

    // Test: Request model-b from Node A (should proxy to Node B)
    let body = serde_json::json!({
        "model": "model-b:default",
        "messages": [{"role": "user", "content": "Hello"}],
        "stream": false
    });
    let resp = client
        .post("http://127.0.0.1:9192/v1/chat/completions")
        .json(&body)
        .send()
        .await
        .unwrap();

    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "Failed to proxy request for model-b via node-a"
    );
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["model"], "model-b:default");

    // Cleanup
    graceful_stop(&mut process_a).await;
    graceful_stop(&mut process_b).await;
    cleanup_procs("mock_server_cluster.sh").await;
    let _ = tokio::fs::remove_file(mock_script).await;
    let _ = tokio::fs::remove_file(config_a_path).await;
    let _ = tokio::fs::remove_file(cookbook_a_path).await;
    let _ = tokio::fs::remove_file(config_b_path).await;
    let _ = tokio::fs::remove_file(cookbook_b_path).await;
}

#[tokio::test]
async fn test_disabled_model() {
    cleanup_procs("mock_server_disabled.sh").await;
    cleanup_procs("config_disabled.yaml").await;
    cleanup_by_port_range_pattern("1303[0-9]").await;
    let root = std::env::current_dir().unwrap();
    let mock_script = setup_mock_script(&root, "disabled").await;
    let config_path = root.join("tests/config_disabled.yaml");
    let cookbook_path = root.join("tests/cookbook_disabled.yaml");
    let mut proxy_bin = root.join("target/release/llamesh");
    if !proxy_bin.exists() {
        let debug_bin = root.join("target/debug/llamesh");
        if debug_bin.exists() {
            proxy_bin = debug_bin;
        }
    }

    let config_content = format!(
        r#"
node_id: "test-node"
listen_addr: "127.0.0.1:9094"
max_vram_mb: 1048576
max_sysmem_mb: 1024
default_model: "enabled-model:default"
model_defaults:
  max_concurrent_requests_per_instance: 2
  max_queue_size_per_model: 10
  max_instances_per_model: 2
  max_wait_in_queue_ms: 5000
llama_cpp_ports:
  ranges:
    - start: 13030
      end: 13039
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
    );

    tokio::fs::write(&config_path, config_content)
        .await
        .unwrap();

    let cookbook_content = r#"
models:
  - name: "enabled-model"
    profiles:
      - id: "default"
        model_path: "./models/mock.gguf"
        idle_timeout_seconds: 5
        max_instances: 1
        llama_server_args: ""
  - name: "disabled-model"
    enabled: false
    profiles:
      - id: "default"
        model_path: "./models/mock-disabled.gguf"
        idle_timeout_seconds: 5
        max_instances: 1
        llama_server_args: ""
"#;
    tokio::fs::write(&cookbook_path, cookbook_content)
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

    assert!(
        wait_for_ready("http://127.0.0.1:9094").await,
        "Proxy failed to become ready"
    );

    let client = reqwest::Client::new();

    // 1. Check models listing - disabled model should NOT be present
    let resp = client
        .get("http://127.0.0.1:9094/v1/models")
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let json: serde_json::Value = resp.json().await.unwrap();

    let data = json["data"].as_array().unwrap();

    // Default profiles are listed with bare model name (not "model:default")
    assert!(data.iter().any(|m| m["id"] == "enabled-model"));
    assert!(!data.iter().any(|m| m["id"] == "disabled-model"));

    // 2. Try to invoke disabled model - should fail
    let body = serde_json::json!({
        "model": "disabled-model:default",
        "messages": [{"role": "user", "content": "Hello"}],
        "stream": false
    });
    let resp = client
        .post("http://127.0.0.1:9094/v1/chat/completions")
        .json(&body)
        .send()
        .await
        .unwrap();

    // Should be 404 Model Not Found
    assert_eq!(resp.status(), StatusCode::NOT_FOUND);

    graceful_stop(&mut proxy_process).await;
    cleanup_procs("mock_server_disabled.sh").await;
    let _ = tokio::fs::remove_file(mock_script).await;
    let _ = tokio::fs::remove_file(config_path).await;
    let _ = tokio::fs::remove_file(cookbook_path).await;
}

#[tokio::test]
async fn test_token_counting() {
    cleanup_procs("mock_server_tokens.sh").await;
    cleanup_procs("config_tokens.yaml").await;
    cleanup_by_port_range_pattern("1304[0-9]").await;
    let root = std::env::current_dir().unwrap();
    let mock_script = setup_mock_script(&root, "tokens").await;
    let config_path = root.join("tests/config_tokens.yaml");
    let cookbook_path = root.join("tests/cookbook_tokens.yaml");
    let mut proxy_bin = root.join("target/release/llamesh");
    if !proxy_bin.exists() {
        let debug_bin = root.join("target/debug/llamesh");
        if debug_bin.exists() {
            proxy_bin = debug_bin;
        }
    }

    let config_content = format!(
        r#"
node_id: "test-node"
listen_addr: "127.0.0.1:9195"
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
    - start: 13040
      end: 13049
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
    );

    tokio::fs::write(&config_path, config_content)
        .await
        .unwrap();

    let cookbook_content = r#"
models:
  - name: "mock-model"
    profiles:
      - id: "default"
        model_path: "./models/mock.gguf"
        idle_timeout_seconds: 5
        max_instances: 1
        llama_server_args: ""
"#;
    tokio::fs::write(&cookbook_path, cookbook_content)
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

    assert!(
        wait_for_ready("http://127.0.0.1:9195").await,
        "Proxy failed to become ready"
    );

    let client = reqwest::Client::new();

    // 1. Non-streamed request
    let body = serde_json::json!({
        "model": "mock-model:default",
        "messages": [{"role": "user", "content": "Hello"}],
        "stream": false
    });
    let resp = client
        .post("http://127.0.0.1:9195/v1/chat/completions")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Wait for background cleanup/metrics update
    sleep(Duration::from_millis(200)).await;

    // Check metrics - expect 10 tokens (mock server hardcoded)
    let metrics_resp = client
        .get("http://127.0.0.1:9195/metrics/json")
        .send()
        .await
        .unwrap();
    let metrics_json: serde_json::Value = metrics_resp.json().await.unwrap();
    // Find hash entry with mock-model:default in display_names
    let hashes = metrics_json["hashes"].as_object().unwrap();
    let hash_entry = hashes
        .values()
        .find(|v| {
            v["display_names"]
                .as_array()
                .map(|arr| arr.iter().any(|n| n.as_str() == Some("mock-model:default")))
                .unwrap_or(false)
        })
        .expect("Should find hash entry for mock-model:default");
    let tokens = hash_entry["tokens_generated_total"].as_u64().unwrap();
    assert_eq!(tokens, 10, "Expected 10 tokens from non-streamed request");

    // 2. Streamed request
    let body = serde_json::json!({
        "model": "mock-model:default",
        "messages": [{"role": "user", "content": "Hello"}],
        "stream": true
    });
    let resp = client
        .post("http://127.0.0.1:9195/v1/chat/completions")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Consume stream
    let _ = resp.bytes().await.unwrap();

    // Check metrics - expect 10 + 9 = 19 tokens (mock server streams 9 words)
    // Wait a bit for background metrics update/cleanup
    sleep(Duration::from_millis(100)).await;

    let metrics_resp = client
        .get("http://127.0.0.1:9195/metrics/json")
        .send()
        .await
        .unwrap();
    let metrics_json: serde_json::Value = metrics_resp.json().await.unwrap();
    // Find hash entry with mock-model:default in display_names
    let hashes = metrics_json["hashes"].as_object().unwrap();
    let hash_entry = hashes
        .values()
        .find(|v| {
            v["display_names"]
                .as_array()
                .map(|arr| arr.iter().any(|n| n.as_str() == Some("mock-model:default")))
                .unwrap_or(false)
        })
        .expect("Should find hash entry for mock-model:default");
    let tokens = hash_entry["tokens_generated_total"].as_u64().unwrap();

    // Mock server streams: "This is a mock response from the dummy server." (9 words)
    // PLUS we now support reading the 'usage' chunk which says 10 tokens.
    assert_eq!(
        tokens, 20,
        "Expected 20 tokens (10 non-stream + 10 stream usage)"
    );

    graceful_stop(&mut proxy_process).await;
    cleanup_procs("mock_server_tokens.sh").await;
    let _ = tokio::fs::remove_file(mock_script).await;
    let _ = tokio::fs::remove_file(config_path).await;
    let _ = tokio::fs::remove_file(cookbook_path).await;
}

#[tokio::test]
async fn test_instance_auth() {
    cleanup_procs("mock_server_auth.sh").await;
    cleanup_procs("config_auth.yaml").await;
    cleanup_by_port_range_pattern("1310[0-9]").await;
    let root = std::env::current_dir().unwrap();
    let mock_script = setup_mock_script(&root, "auth").await;
    let config_path = root.join("tests/config_auth.yaml");
    let cookbook_path = root.join("tests/cookbook_auth.yaml");
    let mut proxy_bin = root.join("target/release/llamesh");
    if !proxy_bin.exists() {
        let debug_bin = root.join("target/debug/llamesh");
        if debug_bin.exists() {
            proxy_bin = debug_bin;
        }
    }

    let config_content = format!(
        r#"
node_id: "test-node"
listen_addr: "127.0.0.1:9197"
max_vram_mb: 1048576
max_sysmem_mb: 1024
default_model: "auth-model:default"
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
    );

    tokio::fs::write(&config_path, config_content)
        .await
        .unwrap();

    let cookbook_content = r#"
models:
  - name: "auth-model"
    profiles:
      - id: "default"
        model_path: "./models/mock.gguf"
        idle_timeout_seconds: 5
        startup_timeout_seconds: 5
        max_instances: 1
        llama_server_args: "--api-key mysecretkey"
"#;
    tokio::fs::write(&cookbook_path, cookbook_content)
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

    // Wait for ready - this triggers instance startup which runs health check
    // If auth fix works, this should succeed. If not, it will timeout/fail.
    assert!(
        wait_for_ready("http://127.0.0.1:9197").await,
        "Proxy failed to become ready (likely auth issue)"
    );

    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "model": "auth-model:default",
        "messages": [{"role": "user", "content": "Hello"}],
        "stream": false
    });

    let resp = client
        .post("http://127.0.0.1:9197/v1/chat/completions")
        .json(&body)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK);

    // Clean up
    graceful_stop(&mut proxy_process).await;
    cleanup_procs("mock_server_auth.sh").await;
    let _ = tokio::fs::remove_file(mock_script).await;
    let _ = tokio::fs::remove_file(config_path).await;
    let _ = tokio::fs::remove_file(cookbook_path).await;
}

#[tokio::test]
async fn test_crash_recovery() {
    cleanup_procs("mock_server_crash.sh").await;
    cleanup_procs("config_crash.yaml").await;
    cleanup_by_port_range_pattern("1305[0-9]").await;
    let root = std::env::current_dir().unwrap();
    let mock_script = setup_mock_script(&root, "crash").await;
    let config_path = root.join("tests/config_crash.yaml");
    let cookbook_path = root.join("tests/cookbook_crash.yaml");
    let mut proxy_bin = root.join("target/release/llamesh");
    if !proxy_bin.exists() {
        let debug_bin = root.join("target/debug/llamesh");
        if debug_bin.exists() {
            proxy_bin = debug_bin;
        }
    }

    let config_content = format!(
        r#"
node_id: "test-node"
listen_addr: "127.0.0.1:9198"
max_vram_mb: 1048576
max_sysmem_mb: 1024
default_model: "crash-model:default"
model_defaults:
  max_concurrent_requests_per_instance: 1
  max_queue_size_per_model: 10
  max_instances_per_model: 1
  max_wait_in_queue_ms: 10000
llama_cpp_ports:
  ranges:
    - start: 13050
      end: 13050
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
    );

    tokio::fs::write(&config_path, config_content)
        .await
        .unwrap();

    let cookbook_content = r#"
models:
  - name: "crash-model"
    profiles:
      - id: "default"
        model_path: "./models/mock.gguf"
        idle_timeout_seconds: 60
        max_instances: 1
        llama_server_args: ""
"#;
    tokio::fs::write(&cookbook_path, cookbook_content)
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

    assert!(
        wait_for_ready("http://127.0.0.1:9198").await,
        "Proxy failed to become ready"
    );

    let client = reqwest::Client::new();

    // 1. Send first request to spawn the instance
    let body = serde_json::json!({
        "model": "crash-model:default",
        "messages": [{"role": "user", "content": "Hello"}],
        "stream": false
    });
    let resp = client
        .post("http://127.0.0.1:9198/v1/chat/completions")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);

    // Instance should be running on 13050.
    // 2. Kill the instance backend
    cleanup_by_port_range_pattern("13050").await;

    // 3. Send another request - should fail or trigger recovery (depending on timing)
    // If logic works, it might fail this request if it tries to reuse the dead connection/instance
    // OR it might detect it's dead and spawn new one.
    // The proxy checks is_alive() before reuse. But is_alive() might be stale (non-blocking wait).
    // Let's see if it fails.

    // Give a tiny bit of time for OS to register process death if needed?
    sleep(Duration::from_millis(100)).await;

    let resp = client
        .post("http://127.0.0.1:9198/v1/chat/completions")
        .json(&body)
        .send()
        .await
        .unwrap();

    // Ideally, the proxy should detect the crash and restart.
    // However, if is_alive() returns true (zombie/not reaped yet or race), request might be forwarded and fail with Bad Gateway.
    // If it fails with 502/500, we retry.

    if resp.status() != StatusCode::OK {
        println!(
            "Request failed as expected (or unexpected), status: {}",
            resp.status()
        );
        // Retry immediately - proxy should have marked it as crashed now
        let resp2 = client
            .post("http://127.0.0.1:9198/v1/chat/completions")
            .json(&body)
            .send()
            .await
            .unwrap();
        assert_eq!(resp2.status(), StatusCode::OK, "Recovery request failed");
    } else {
        println!("Request succeeded immediately (recovery was fast/transparent)");
    }

    graceful_stop(&mut proxy_process).await;
    cleanup_procs("mock_server_crash.sh").await;
    let _ = tokio::fs::remove_file(mock_script).await;
    let _ = tokio::fs::remove_file(config_path).await;
    let _ = tokio::fs::remove_file(cookbook_path).await;
}

#[tokio::test]
async fn test_queueing() {
    cleanup_procs("mock_server_queue.sh").await;
    cleanup_procs("config_queue.yaml").await;
    cleanup_by_port_range_pattern("1306[0-9]").await;
    let root = std::env::current_dir().unwrap();
    let mock_script = setup_mock_script(&root, "queue").await;
    let config_path = root.join("tests/config_queue.yaml");
    let cookbook_path = root.join("tests/cookbook_queue.yaml");
    let mut proxy_bin = root.join("target/release/llamesh");
    if !proxy_bin.exists() {
        let debug_bin = root.join("target/debug/llamesh");
        if debug_bin.exists() {
            proxy_bin = debug_bin;
        }
    }

    let config_content = format!(
        r#"
node_id: "test-node"
listen_addr: "127.0.0.1:9199"
max_vram_mb: 1048576
max_sysmem_mb: 1024
default_model: "queue-model:default"
model_defaults:
  max_concurrent_requests_per_instance: 1
  max_queue_size_per_model: 10
  max_instances_per_model: 1
  max_wait_in_queue_ms: 10000
llama_cpp_ports:
  ranges:
    - start: 13060
      end: 13060
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
    );

    tokio::fs::write(&config_path, config_content)
        .await
        .unwrap();

    let cookbook_content = r#"
models:
  - name: "queue-model"
    profiles:
      - id: "default"
        model_path: "./models/mock.gguf"
        idle_timeout_seconds: 60
        max_instances: 1
        llama_server_args: ""
"#;
    tokio::fs::write(&cookbook_path, cookbook_content)
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

    assert!(
        wait_for_ready("http://127.0.0.1:9199").await,
        "Proxy failed to become ready"
    );

    let client = reqwest::Client::new();
    let body = serde_json::json!({
        "model": "queue-model:default",
        "messages": [{"role": "user", "content": "Hello"}],
        "stream": false
    });

    // We need simultaneous requests.
    // Since mock server processes in serial (1 slot) and we configured max_concurrent_requests_per_instance: 1,
    // the second request must wait.

    let client1 = client.clone();
    let body1 = body.clone();
    let task1 = tokio::spawn(async move {
        let start = std::time::Instant::now();
        let resp = client1
            .post("http://127.0.0.1:9199/v1/chat/completions")
            .json(&body1)
            .send()
            .await
            .unwrap();
        (start.elapsed(), resp.status())
    });

    // Slight delay to ensure order
    sleep(Duration::from_millis(50)).await;

    let client2 = client.clone();
    let body2 = body.clone();
    let task2 = tokio::spawn(async move {
        let start = std::time::Instant::now();
        let resp = client2
            .post("http://127.0.0.1:9199/v1/chat/completions")
            .json(&body2)
            .send()
            .await
            .unwrap();
        (start.elapsed(), resp.status())
    });

    let (res1, res2) = tokio::join!(task1, task2);
    let (dur1, status1) = res1.unwrap();
    let (dur2, status2) = res2.unwrap();

    assert_eq!(status1, StatusCode::OK);
    assert_eq!(status2, StatusCode::OK);

    println!("Req 1 duration: {:?}", dur1);
    println!("Req 2 duration: {:?}", dur2);

    // Mock server delays ~100-500ms + 500-2000ms.
    // Req 2 should be roughly 2x Req 1 duration if queued, or at least significantly longer than Req 1 if they were parallel (but they aren't parallel).
    // Actually, if they were parallel on server side, they would take similar time.
    // If queued, Req 2 starts after Req 1 finishes.
    // So Dur 2 >= Dur 1 (approx).
    // The key is that Req 2 didn't fail with "No Capacity".

    assert!(
        dur2 > Duration::from_millis(500),
        "Request 2 should take some time"
    );

    graceful_stop(&mut proxy_process).await;
    cleanup_procs("mock_server_queue.sh").await;
    let _ = tokio::fs::remove_file(mock_script).await;
    let _ = tokio::fs::remove_file(config_path).await;
    let _ = tokio::fs::remove_file(cookbook_path).await;
}

/// Tests that the hop counter validation rejects malformed headers
#[tokio::test]
async fn test_hop_counter_validation() {
    cleanup_procs("mock_server_hops.sh").await;
    cleanup_procs("config_hops.yaml").await;
    cleanup_by_port_range_pattern("1311[0-9]").await;
    let root = std::env::current_dir().unwrap();
    let mock_script = setup_mock_script(&root, "hops").await;
    let config_path = root.join("tests/config_hops.yaml");
    let cookbook_path = root.join("tests/cookbook_hops.yaml");
    let mut proxy_bin = root.join("target/release/llamesh");
    if !proxy_bin.exists() {
        let debug_bin = root.join("target/debug/llamesh");
        if debug_bin.exists() {
            proxy_bin = debug_bin;
        }
    }

    let config_content = format!(
        r#"
node_id: "test-node-hops"
listen_addr: "127.0.0.1:9201"
max_vram_mb: 1048576
max_sysmem_mb: 1024
default_model: "mock-model:default"
max_hops: 5
model_defaults:
  max_concurrent_requests_per_instance: 2
  max_queue_size_per_model: 10
  max_instances_per_model: 2
  max_wait_in_queue_ms: 5000
llama_cpp_ports:
  ranges:
    - start: 13110
      end: 13119
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
    );

    tokio::fs::write(&config_path, config_content)
        .await
        .unwrap();

    let cookbook_content = r#"
models:
  - name: "mock-model"
    description: "Mock model for testing"
    profiles:
      - id: "default"
        model_path: "./models/mock.gguf"
        idle_timeout_seconds: 5
        max_instances: 1
        llama_server_args: ""
"#;
    tokio::fs::write(&cookbook_path, cookbook_content)
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

    assert!(
        wait_for_ready("http://127.0.0.1:9201").await,
        "Proxy failed to become ready"
    );

    let client = reqwest::Client::new();

    // Test 1: Malformed hop header (non-integer) should return 400
    let body = serde_json::json!({
        "model": "mock-model:default",
        "messages": [{"role": "user", "content": "Hi"}]
    });
    let resp = client
        .post("http://127.0.0.1:9201/v1/chat/completions")
        .header("x-llama-mesh-hops", "invalid")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "Malformed hop header should return 400"
    );

    // Test 2: Hop count exceeding max_hops should return 400
    let resp = client
        .post("http://127.0.0.1:9201/v1/chat/completions")
        .header("x-llama-mesh-hops", "10")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::BAD_REQUEST,
        "Hop count exceeding max_hops should return 400"
    );

    // Test 3: Valid hop count (0) should work
    let resp = client
        .post("http://127.0.0.1:9201/v1/chat/completions")
        .header("x-llama-mesh-hops", "0")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "Valid hop count should return 200"
    );

    // Test 4: No hop header should work (defaults to 0)
    let resp = client
        .post("http://127.0.0.1:9201/v1/chat/completions")
        .json(&body)
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "No hop header should work (defaults to 0)"
    );

    graceful_stop(&mut proxy_process).await;
    cleanup_procs("mock_server_hops.sh").await;
    let _ = tokio::fs::remove_file(mock_script).await;
    let _ = tokio::fs::remove_file(config_path).await;
    let _ = tokio::fs::remove_file(cookbook_path).await;
}
