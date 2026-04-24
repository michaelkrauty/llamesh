use reqwest::StatusCode;

mod common;
use common::{cleanup_procs, graceful_stop, llamesh_binary, setup_mock_script, wait_for_ready};

#[tokio::test]
async fn test_admin_auth() {
    cleanup_procs("mock_server_admin_auth.sh").await;
    cleanup_procs("config_admin_auth.yaml").await;

    let root = std::env::current_dir().unwrap();
    let mock_script = setup_mock_script(&root, "admin_auth").await;
    let config_path = root.join("tests/config_admin_auth.yaml");
    let cookbook_path = root.join("tests/cookbook_admin_auth.yaml");
    let proxy_bin = llamesh_binary(&root);

    let config_content = format!(
        r#"
node_id: "test-node"
listen_addr: "127.0.0.1:9200"
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
    - start: 13200
      end: 13209
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
auth:
  enabled: true
  required_header: "x-api-key"
  allowed_keys: ["adminsecret"]
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
        wait_for_ready("http://127.0.0.1:9200").await,
        "Proxy failed to become ready"
    );

    let client = reqwest::Client::new();

    // 1. Access Cluster Nodes without Key
    let resp = client
        .get("http://127.0.0.1:9200/cluster/nodes")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "/cluster/nodes should require auth"
    );

    // 2. Access Cluster Nodes WITH Key
    let resp = client
        .get("http://127.0.0.1:9200/cluster/nodes")
        .header("x-api-key", "adminsecret")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::OK,
        "/cluster/nodes with key should succeed"
    );

    // 3. Access Prewarm without Key
    let resp = client
        .post("http://127.0.0.1:9200/admin/prewarm")
        .json(&serde_json::json!({"model": "mock-model:default"}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "/admin/prewarm should require auth"
    );

    // 4. Access Prewarm WITH Key
    let resp = client
        .post("http://127.0.0.1:9200/admin/prewarm")
        .header("x-api-key", "adminsecret")
        .json(&serde_json::json!({"model": "mock-model:default"}))
        .send()
        .await
        .unwrap();
    // Assuming prewarm works if model exists (it should spawn instance)
    // Or at least it returns accepted/ok
    assert!(
        resp.status().is_success(),
        "/admin/prewarm with key should succeed"
    );

    // 5. Rebuild without Key
    let resp = client
        .post("http://127.0.0.1:9200/admin/rebuild-llama")
        .send()
        .await
        .unwrap();
    assert_eq!(
        resp.status(),
        StatusCode::UNAUTHORIZED,
        "/admin/rebuild-llama should require auth"
    );

    // 6. Rebuild with Key
    // Note: Rebuild might fail internally because repo is invalid/empty in this mock config, but the AUTH check happens before that.
    // And we return ACCEPTED if lock is acquired.
    let resp = client
        .post("http://127.0.0.1:9200/admin/rebuild-llama")
        .header("x-api-key", "adminsecret")
        .send()
        .await
        .unwrap();
    // It might return 202 Accepted or 409 Conflict (if prewarm triggered something? prewarm shouldn't lock rebuild)
    // Actually build_manager might be running from prewarm? No, prewarm just spawns instance.
    // Rebuild lock is separate.
    assert!(
        resp.status() == StatusCode::ACCEPTED || resp.status() == StatusCode::CONFLICT,
        "Rebuild with key should be accepted or conflict (if busy), but not unauthorized. Got: {}",
        resp.status()
    );

    graceful_stop(&mut proxy_process).await;
    cleanup_procs("mock_server_admin_auth.sh").await;
    let _ = tokio::fs::remove_file(mock_script).await;
    let _ = tokio::fs::remove_file(config_path).await;
    let _ = tokio::fs::remove_file(cookbook_path).await;
}
