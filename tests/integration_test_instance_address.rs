//! A profile can set `--host` to an IPv6 address, which `llama-server` takes
//! without brackets. The proxy must probe and forward to such an instance
//! through a valid URL, bracketing the address, and reach an instance that
//! listens on the unspecified address `::` through IPv6 loopback.

use std::time::Duration;

mod common;
use common::{
    cleanup_by_port_range_pattern, cleanup_procs, graceful_stop, llamesh_binary, setup_mock_script,
    wait_for_ready,
};

const LISTEN_ADDR: &str = "127.0.0.1:9261";
const BASE_URL: &str = "http://127.0.0.1:9261";
const SUFFIX: &str = "instance_address";

fn config_yaml(mock_script: &std::path::Path) -> String {
    format!(
        r#"
node_id: "test-{SUFFIX}"
listen_addr: "{LISTEN_ADDR}"
metrics_path: "./tests/metrics_{SUFFIX}.json"
max_vram_mb: 1048576
max_sysmem_mb: 1024
default_model: "mock-model:loopback"
model_defaults:
  max_concurrent_requests_per_instance: 2
  max_queue_size_per_model: 10
  max_instances_per_model: 1
  max_wait_in_queue_ms: 5000
llama_cpp_ports:
  ranges:
    - start: 13310
      end: 13319
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
      - id: "loopback"
        model_path: "./models/mock.gguf"
        idle_timeout_seconds: 60
        startup_timeout_seconds: 30
        llama_server_args: "--host ::1"
      - id: "unspecified"
        model_path: "./models/mock.gguf"
        idle_timeout_seconds: 60
        startup_timeout_seconds: 30
        llama_server_args: "--host ::"
"#;

#[tokio::test]
async fn instances_listening_on_ipv6_addresses_serve_requests() {
    if std::net::TcpListener::bind("[::1]:0").is_err() {
        eprintln!("skipping: IPv6 loopback is not available");
        return;
    }

    cleanup_procs("mock_server_instance_address.sh").await;
    cleanup_procs("config_instance_address.yaml").await;
    cleanup_by_port_range_pattern("1331[0-9]").await;

    let root = std::env::current_dir().unwrap();
    let mock_script = setup_mock_script(&root, SUFFIX).await;
    let metrics_path = common::reset_metrics_file(&root, SUFFIX).await;
    let config_path = root.join(format!("tests/config_{SUFFIX}.yaml"));
    let cookbook_path = root.join(format!("tests/cookbook_{SUFFIX}.yaml"));
    tokio::fs::write(&config_path, config_yaml(&mock_script))
        .await
        .unwrap();
    tokio::fs::write(&cookbook_path, COOKBOOK).await.unwrap();

    let mut proxy_process = tokio::process::Command::new(llamesh_binary(&root))
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
        .timeout(Duration::from_secs(60))
        .build()
        .unwrap();
    for profile in ["loopback", "unspecified"] {
        for stream in [false, true] {
            let response = client
                .post(format!("{BASE_URL}/v1/chat/completions"))
                .json(&serde_json::json!({
                    "model": format!("mock-model:{profile}"),
                    "messages": [{"role": "user", "content": "hi"}],
                    "stream": stream,
                }))
                .send()
                .await
                .unwrap_or_else(|e| panic!("{profile} (stream: {stream}): {e}"));
            let status = response.status();
            let body = response.text().await.unwrap();
            assert_eq!(status, 200, "{profile} (stream: {stream}): {body}");
            let marker = if stream { "[DONE]" } else { "\"choices\"" };
            assert!(
                body.contains(marker),
                "{profile} (stream: {stream}): incomplete body: {body}"
            );
        }
    }

    graceful_stop(&mut proxy_process).await;
    cleanup_procs("mock_server_instance_address.sh").await;
    cleanup_by_port_range_pattern("1331[0-9]").await;
    for path in [mock_script, config_path, cookbook_path, metrics_path] {
        let _ = tokio::fs::remove_file(path).await;
    }
}
