use reqwest::{StatusCode, Version};

mod common;
use common::{
    cleanup_by_port_range_pattern, cleanup_procs, graceful_stop, llamesh_binary, setup_mock_script,
    wait_for_ready,
};

/// End-to-end coverage for prior-knowledge cleartext HTTP/2 (h2c): the protocol
/// detector classifies the `PRI ` preface as `DetectedProtocol::Http2` and the
/// connection must then be served by the HTTP/2 stack. Exercises a plain GET, a
/// non-streaming chat completion, and an SSE streaming chat completion, all
/// multiplexed over h2c connections from a single prior-knowledge client.
#[tokio::test]
async fn test_h2c_prior_knowledge() {
    cleanup_procs("mock_server_h2c.sh").await;
    cleanup_procs("config_h2c.yaml").await;
    cleanup_by_port_range_pattern("1321[0-9]").await;
    let root = std::env::current_dir().unwrap();
    let mock_script = setup_mock_script(&root, "h2c").await;
    let config_path = root.join("tests/config_h2c.yaml");
    let cookbook_path = root.join("tests/cookbook_h2c.yaml");
    let proxy_bin = llamesh_binary(&root);

    let config_content = format!(
        r#"
node_id: "test-node-h2c"
listen_addr: "127.0.0.1:9202"
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
    - start: 13210
      end: 13219
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
        wait_for_ready("http://127.0.0.1:9202").await,
        "Proxy failed to become ready"
    );

    // Every request from this client is prior-knowledge cleartext HTTP/2:
    // the first bytes on the wire are the `PRI ` connection preface.
    let h2c_client = reqwest::Client::builder()
        .http2_prior_knowledge()
        .build()
        .unwrap();

    // 1. Plain GET over h2c
    let resp = h2c_client
        .get("http://127.0.0.1:9202/version")
        .send()
        .await
        .expect("h2c GET /version failed");
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.version(), Version::HTTP_2, "response is not HTTP/2");
    let body: serde_json::Value = resp.json().await.unwrap();
    assert!(body["version"].is_string());

    // 2. Non-streaming chat completion over h2c
    let body = serde_json::json!({
        "model": "mock-model:default",
        "messages": [{"role": "user", "content": "Hello"}],
        "stream": false
    });
    let resp = h2c_client
        .post("http://127.0.0.1:9202/v1/chat/completions")
        .json(&body)
        .send()
        .await
        .expect("h2c chat completion failed");
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.version(), Version::HTTP_2, "response is not HTTP/2");
    let completion: serde_json::Value = resp.json().await.unwrap();
    assert!(
        completion["choices"][0]["message"]["content"].is_string(),
        "unexpected completion body: {completion}"
    );

    // 3. SSE streaming chat completion over h2c
    let body = serde_json::json!({
        "model": "mock-model:default",
        "messages": [{"role": "user", "content": "Hello"}],
        "stream": true
    });
    let resp = h2c_client
        .post("http://127.0.0.1:9202/v1/chat/completions")
        .json(&body)
        .send()
        .await
        .expect("h2c streaming chat completion failed");
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.version(), Version::HTTP_2, "response is not HTTP/2");
    let stream_body = resp.text().await.unwrap();
    assert!(
        stream_body.contains("data:"),
        "expected SSE data frames, got: {stream_body}"
    );
    assert!(
        stream_body.contains("[DONE]"),
        "expected SSE terminator, got: {stream_body}"
    );

    // 4. HTTP/1.1 still served on the same port (protocol multiplexing intact)
    let h1_client = reqwest::Client::new();
    let resp = h1_client
        .get("http://127.0.0.1:9202/version")
        .send()
        .await
        .expect("HTTP/1.1 GET /version failed");
    assert_eq!(resp.status(), StatusCode::OK);
    assert_eq!(resp.version(), Version::HTTP_11, "response is not HTTP/1.1");

    graceful_stop(&mut proxy_process).await;
    let _ = tokio::fs::remove_file(&config_path).await;
    let _ = tokio::fs::remove_file(&cookbook_path).await;
    let _ = tokio::fs::remove_file(&mock_script).await;
}
