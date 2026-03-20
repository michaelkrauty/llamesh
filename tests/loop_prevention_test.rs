mod common;
use common::*;
use reqwest::StatusCode;

async fn start_mock_node(suffix: &str, port_offset: u16) -> (tokio::process::Child, String) {
    let id = format!("loop-{}", suffix);
    let port = 14000 + port_offset;
    let listen = format!("127.0.0.1:{}", port);

    cleanup_procs(&format!("config_{}.yaml", id)).await;
    cleanup_by_port_range_pattern(&format!("150{}", port_offset)).await; // Mock server ports

    let root = std::env::current_dir().unwrap();
    let mock_script = setup_mock_script(&root, &id).await;
    let config_path = root.join(format!("tests/config_{}.yaml", id));
    let cookbook_path = root.join(format!("tests/cookbook_{}.yaml", id));
    let mut proxy_bin = root.join("target/release/llamesh");
    if !proxy_bin.exists() {
        let debug_bin = root.join("target/debug/llamesh");
        if debug_bin.exists() {
            proxy_bin = debug_bin;
        }
    }

    let config_content = format!(
        r#"
node_id: "{}"
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
    - start: {}
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
        id,
        listen,
        15000 + port_offset,
        15009 + port_offset,
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

    let proxy_process = tokio::process::Command::new(&proxy_bin)
        .arg("--config")
        .arg(&config_path)
        .arg("--cookbook")
        .arg(&cookbook_path)
        .kill_on_drop(true)
        .spawn()
        .expect("Failed to start proxy");

    let addr = format!("http://{}", listen);
    assert!(wait_for_ready(&addr).await, "Proxy failed to become ready");

    (proxy_process, addr)
}

#[tokio::test]
async fn test_loop_prevention_rejects_too_many_hops() {
    let (mut proc, addr) = start_mock_node("too-many", 0).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{}/v1/chat/completions", addr))
        .header("X-Llama-Mesh-Hops", "11")
        .json(&serde_json::json!({
            "model": "mock-model:default",
            "messages": []
        }))
        .send()
        .await
        .expect("Failed to send request");

    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    let body: serde_json::Value = resp.json().await.expect("Failed to parse JSON");
    assert_eq!(body["error"]["type"], "invalid_request_error");
    assert!(body["error"]["message"]
        .as_str()
        .unwrap()
        .contains("Too many hops"));

    graceful_stop(&mut proc).await;
}

#[tokio::test]
async fn test_loop_prevention_allows_few_hops() {
    let (mut proc, addr) = start_mock_node("few-hops", 10).await;
    let client = reqwest::Client::new();

    let resp = client
        .post(format!("{}/v1/chat/completions", addr))
        .header("X-Llama-Mesh-Hops", "5")
        .json(&serde_json::json!({
            "model": "mock-model:default",
            "messages": [{"role": "user", "content": "hi"}]
        }))
        .send()
        .await
        .expect("Failed to send request");

    assert_eq!(resp.status(), StatusCode::OK);

    graceful_stop(&mut proc).await;
}
