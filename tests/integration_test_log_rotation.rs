//! Daily log files follow UTC days, matching the UTC timestamps of their
//! entries, whatever the host's time zone.
//!
//! The node runs with `TZ` set to an offset whose local date differs from the
//! UTC date at the time of the test (UTC+14 from 12:00 UTC on, UTC-12 before),
//! so a file named by local date would be detected. The current UTC day's file
//! is also present only in compressed form, as if it had already been rotated:
//! the node must restore and continue it, because rotating it again would
//! overwrite the compressed file and lose its entries.

use chrono::{FixedOffset, Timelike, Utc};
use std::io::{Read, Write};
use std::time::Duration;

mod common;
use common::{graceful_stop, llamesh_binary, reset_metrics_file, wait_for_ready};

const LISTEN_ADDR: &str = "127.0.0.1:9253";
const BASE_URL: &str = "http://127.0.0.1:9253";
const SUFFIX: &str = "log_rotation";
const EARLIER_ENTRY: &str = r#"{"timestamp":"earlier","level":"INFO","fields":{"message":"entry written before the restart"}}"#;

fn config_yaml(log_dir: &std::path::Path) -> String {
    format!(
        r#"
node_id: "test-{SUFFIX}"
listen_addr: "{LISTEN_ADDR}"
metrics_path: "./tests/metrics_{SUFFIX}.json"
max_vram_mb: 1048576
max_sysmem_mb: 1024
default_model: "mock-model:default"
model_defaults:
  max_concurrent_requests_per_instance: 1
  max_queue_size_per_model: 1
  max_instances_per_model: 1
  max_wait_in_queue_ms: 1000
llama_cpp_ports:
  ranges:
    - start: 13300
      end: 13309
llama_cpp:
  repo_url: ""
  repo_path: "."
  build_path: "."
  binary_path: "/bin/false"
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
logging:
  enabled: true
  directory: "{}"
  filename: "proxy.log"
  max_keep_files: 7
  compression: true
"#,
        log_dir.display()
    )
}

const COOKBOOK: &str = r#"
models:
  - name: "mock-model"
    enabled: true
    profiles:
      - id: "default"
        model_path: "./models/mock.gguf"
        idle_timeout_seconds: 60
        max_instances: 1
        llama_server_args: ""
"#;

#[tokio::test]
async fn log_files_follow_utc_days() {
    let now = Utc::now();
    let (tz, offset_hours) = if now.hour() >= 12 {
        ("<+14>-14", 14)
    } else {
        ("<-12>+12", -12)
    };
    let utc_day = now.format("%Y-%m-%d").to_string();
    let local_day = now
        .with_timezone(&FixedOffset::east_opt(offset_hours * 3600).unwrap())
        .format("%Y-%m-%d")
        .to_string();
    assert_ne!(utc_day, local_day, "the chosen offset must change the date");

    let root = std::env::current_dir().unwrap();
    let log_dir = root.join(format!("tests/logs_{SUFFIX}"));
    let _ = tokio::fs::remove_dir_all(&log_dir).await;
    tokio::fs::create_dir_all(&log_dir).await.unwrap();
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    writeln!(encoder, "{EARLIER_ENTRY}").unwrap();
    tokio::fs::write(
        log_dir.join(format!("proxy.log.{utc_day}.gz")),
        encoder.finish().unwrap(),
    )
    .await
    .unwrap();

    let metrics_path = reset_metrics_file(&root, SUFFIX).await;
    let config_path = root.join(format!("tests/config_{SUFFIX}.yaml"));
    let cookbook_path = root.join(format!("tests/cookbook_{SUFFIX}.yaml"));
    tokio::fs::write(&config_path, config_yaml(&log_dir))
        .await
        .unwrap();
    tokio::fs::write(&cookbook_path, COOKBOOK).await.unwrap();

    let mut proxy_process = tokio::process::Command::new(llamesh_binary(&root))
        .arg("--config")
        .arg(&config_path)
        .arg("--cookbook")
        .arg(&cookbook_path)
        .env("TZ", tz)
        .kill_on_drop(true)
        .spawn()
        .expect("failed to start proxy");
    let ready = wait_for_ready(BASE_URL).await;
    graceful_stop(&mut proxy_process).await;
    // The file writer is non-blocking; give it a moment to drain.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let mut names: Vec<String> = std::fs::read_dir(&log_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    names.sort();
    let mut contents = String::new();
    if let Ok(mut file) = std::fs::File::open(log_dir.join(format!("proxy.log.{utc_day}"))) {
        file.read_to_string(&mut contents).unwrap();
    }

    for path in [&config_path, &cookbook_path, &metrics_path] {
        let _ = tokio::fs::remove_file(path).await;
    }
    let _ = tokio::fs::remove_dir_all(&log_dir).await;

    assert!(ready, "proxy failed to become ready");
    assert_eq!(
        names,
        [format!("proxy.log.{utc_day}")],
        "expected only the UTC day's file (local day: {local_day})"
    );
    let mut lines = contents.lines();
    assert_eq!(
        lines.next(),
        Some(EARLIER_ENTRY),
        "the restored day's earlier entries must come first"
    );
    let started: serde_json::Value = lines
        .find(|line| line.contains("Starting llamesh"))
        .map(|line| serde_json::from_str(line).unwrap())
        .expect("the restarted node's entries must follow");
    assert!(
        started["timestamp"].as_str().unwrap().starts_with(&utc_day),
        "entry timestamps must fall on the file's day: {started}"
    );
}
