use reqwest::StatusCode;

mod common;
use common::{
    cleanup_by_port_range_pattern, cleanup_procs, graceful_stop, llamesh_binary, setup_mock_script,
    wait_for_ready,
};

#[tokio::test]
async fn test_embeddings_and_rerank() {
    cleanup_procs("mock_server_features.sh").await;
    cleanup_procs("config_features.yaml").await;
    cleanup_by_port_range_pattern("1307[0-9]").await;
    let root = std::env::current_dir().unwrap();
    let mock_script = setup_mock_script(&root, "features").await;
    let config_path = root.join("tests/config_features.yaml");
    let cookbook_path = root.join("tests/cookbook_features.yaml");
    let proxy_bin = llamesh_binary(&root);

    let config_content = format!(
        r#"
node_id: "test-node"
listen_addr: "127.0.0.1:9200"
max_vram_mb: 1048576
max_sysmem_mb: 1024
default_model: "embedding-model:default"
model_defaults:
  max_concurrent_requests_per_instance: 2
  max_queue_size_per_model: 10
  max_instances_per_model: 2
  max_wait_in_queue_ms: 5000
llama_cpp_ports:
  ranges:
    - start: 13070
      end: 13079
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
  - name: "embedding-model"
    profiles:
      - id: "default"
        model_path: "./models/mock-emb.gguf"
        idle_timeout_seconds: 60
        max_instances: 1
        llama_server_args: "--embedding"

  - name: "rerank-model"
    profiles:
      - id: "default"
        model_path: "./models/mock-rerank.gguf"
        idle_timeout_seconds: 60
        max_instances: 1
        llama_server_args: "--rerank"
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
        wait_for_ready("http://127.0.0.1:9200").await,
        "Proxy failed to become ready"
    );

    let client = reqwest::Client::new();

    // 1. Test Embeddings
    let body = serde_json::json!({
        "model": "embedding-model:default",
        "input": "Hello world"
    });
    let resp = client
        .post("http://127.0.0.1:9200/v1/embeddings")
        .json(&body)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK, "Embeddings request failed");
    let json: serde_json::Value = resp.json().await.unwrap();
    assert_eq!(json["object"], "list");
    assert_eq!(json["data"][0]["object"], "embedding");

    // 2. Test Reranking
    let body = serde_json::json!({
        "model": "rerank-model:default",
        "query": "What is love?",
        "documents": ["Baby don't hurt me", "Don't hurt me", "No more"]
    });
    // Note: The proxy exposes /v1/rerank or /rerank. Let's try /v1/rerank
    let resp = client
        .post("http://127.0.0.1:9200/v1/rerank")
        .json(&body)
        .send()
        .await
        .unwrap();

    assert_eq!(resp.status(), StatusCode::OK, "Rerank request failed");
    let json: serde_json::Value = resp.json().await.unwrap();
    assert!(json["results"].is_array());

    // 3. Test Wrong Endpoint (Text model on embedding endpoint)
    // "embedding-model" is configured for embeddings. Try using it for chat.
    let body = serde_json::json!({
        "model": "embedding-model:default",
        "messages": [{"role": "user", "content": "Hello"}]
    });
    let resp = client
        .post("http://127.0.0.1:9200/v1/chat/completions")
        .json(&body)
        .send()
        .await
        .unwrap();

    // Should fail with 400 Bad Request because profile restricts to embeddings
    assert_eq!(resp.status(), StatusCode::BAD_REQUEST);

    graceful_stop(&mut proxy_process).await;
    cleanup_procs("mock_server_features.sh").await;
    let _ = tokio::fs::remove_file(mock_script).await;
    let _ = tokio::fs::remove_file(config_path).await;
    let _ = tokio::fs::remove_file(cookbook_path).await;
}
