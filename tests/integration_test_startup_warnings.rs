//! Warnings that validating the configuration and cookbook emits at startup
//! must reach the log. Startup validates before initializing logging, so an
//! invalid configuration exits before the log files are touched, but tracing
//! drops events emitted while no subscriber is installed; these warnings used
//! to appear only when a cookbook reload validated again.

use std::time::Duration;

mod common;
use common::{graceful_stop, llamesh_binary};

const PORT: u16 = 9262;

#[tokio::test]
async fn startup_validation_warnings_are_logged() {
    let root = std::env::current_dir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    let config = format!(
        r#"
node_id: "startup-warnings"
listen_addr: "127.0.0.1:{PORT}"
metrics_path: "./metrics.json"
max_vram_mb: 1024
max_sysmem_mb: 1024
default_model: "warned-model"
model_defaults:
  max_concurrent_requests_per_instance: 1
  max_queue_size_per_model: 1
  max_instances_per_model: 1
  max_wait_in_queue_ms: 1000
llama_cpp:
  repo_url: ""
  repo_path: "."
  build_path: "."
  binary_path: "./missing-llama-server"
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
"#
    );
    // Validation warns about an hf_file without a Hugging Face repository,
    // and about profile fields that a model source in the args overrides.
    let cookbook = r#"
models:
  - name: "warned-model"
    profiles:
      - id: "default"
        model_path: "./model.gguf"
        hf_file: "model-q4_k_m.gguf"
        llama_server_args: ""
      - id: "overridden"
        model_path: "./model.gguf"
        llama_server_args: "-m ./other.gguf"
"#;
    std::fs::write(dir.path().join("config.yaml"), config).unwrap();
    std::fs::write(dir.path().join("cookbook.yaml"), cookbook).unwrap();
    let output_path = dir.path().join("proxy.log");
    let output = std::fs::File::create(&output_path).unwrap();

    let mut proxy = tokio::process::Command::new(llamesh_binary(&root))
        .args(["--config", "config.yaml", "--cookbook", "cookbook.yaml"])
        .current_dir(dir.path())
        .env("RUST_LOG", "info")
        .stdout(output.try_clone().unwrap())
        .stderr(output)
        .kill_on_drop(true)
        .spawn()
        .expect("failed to start proxy");

    // The node answers /healthz (with 503, having no binary) once startup,
    // including validation, is done.
    let client = reqwest::Client::new();
    let mut up = false;
    for _ in 0..100 {
        if client
            .get(format!("http://127.0.0.1:{PORT}/healthz"))
            .send()
            .await
            .is_ok()
        {
            up = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    graceful_stop(&mut proxy).await;

    let log = std::fs::read_to_string(&output_path).unwrap();
    assert!(up, "proxy did not start: {log}");
    let warned = |text: &str| {
        log.lines()
            .filter_map(|line| serde_json::from_str::<serde_json::Value>(line).ok())
            .any(|entry| {
                entry["level"] == "WARN"
                    && entry["fields"]["message"]
                        .as_str()
                        .is_some_and(|message| message.contains(text))
            })
    };
    assert!(
        warned("'hf_file' will be ignored"),
        "startup warning missing from the log: {log}"
    );
    assert!(
        warned("Profile warned-model:overridden sets its model source in llama_server_args"),
        "startup warning missing from the log: {log}"
    );
}

#[tokio::test]
async fn invalid_config_exits_before_preparing_log_files() {
    let root = std::env::current_dir().unwrap();
    let dir = tempfile::tempdir().unwrap();
    // Enabled API key auth without any allowed key fails validation.
    let config = r#"
node_id: "invalid-config"
listen_addr: "127.0.0.1:9263"
metrics_path: "./metrics.json"
max_vram_mb: 1024
max_sysmem_mb: 1024
default_model: "m"
model_defaults:
  max_concurrent_requests_per_instance: 1
  max_queue_size_per_model: 1
  max_instances_per_model: 1
  max_wait_in_queue_ms: 1000
llama_cpp:
  repo_url: ""
  repo_path: "."
  build_path: "."
  binary_path: "./missing-llama-server"
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
  allowed_keys: []
logging:
  enabled: true
  directory: "./logs"
"#;
    std::fs::write(dir.path().join("config.yaml"), config).unwrap();
    std::fs::write(dir.path().join("cookbook.yaml"), "models: []\n").unwrap();

    let output = tokio::time::timeout(
        Duration::from_secs(10),
        tokio::process::Command::new(llamesh_binary(&root))
            .args(["--config", "config.yaml", "--cookbook", "cookbook.yaml"])
            .current_dir(dir.path())
            .kill_on_drop(true)
            .output(),
    )
    .await
    .expect("proxy did not exit")
    .unwrap();

    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(!output.status.success());
    assert!(stderr.contains("Invalid config"), "{stderr}");
    assert!(
        !dir.path().join("logs").exists(),
        "logging prepared the log directory for an invalid config"
    );
}
