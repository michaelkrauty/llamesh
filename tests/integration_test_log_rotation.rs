//! Daily log files follow UTC days, matching the UTC timestamps of their
//! entries, whatever the host's time zone.
//!
//! The node runs with `TZ` set to an offset whose local date differs from the
//! UTC date at the time of the test (UTC+14 from 12:00 UTC on, UTC-12 before),
//! so a file named by local date would be detected. The log directory is laid
//! out as a version that rotated on local days leaves it on a host east of UTC
//! between local and UTC midnight: the current UTC day's file was already
//! rotated and exists only in compressed form, and the entries written since
//! local midnight, stamped on the current UTC day, are in a file named for the
//! next day. The node must continue the current day's file with both sets of
//! entries in order, rather than overwrite the compressed file at the next
//! rotation or append the next day's entries to the misnamed file.

use chrono::{FixedOffset, Timelike, Utc};
use std::io::Write;
use std::path::Path;

mod common;
use common::{graceful_stop, llamesh_binary, reset_metrics_file, wait_for_ready};

const LISTEN_ADDR: &str = "127.0.0.1:9253";
const BASE_URL: &str = "http://127.0.0.1:9253";
const SUFFIX: &str = "log_rotation";
const COMPRESSED_ENTRY: &str = r#"{"timestamp":"compressed","level":"INFO","fields":{"message":"entry from the rotated file"}}"#;

fn config_yaml(log_dir: &Path) -> String {
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

struct Run {
    utc_day: String,
    local_day: String,
    next_day_entry: String,
    ready: bool,
    files: Vec<String>,
    contents: String,
}

/// Lay out the log directory, run the node until it is ready, stop it, and
/// collect the resulting log files.
async fn run_node(root: &Path) -> Run {
    let now = Utc::now();
    let (tz, offset_hours) = if now.hour() >= 12 {
        ("<+14>-14", 14)
    } else {
        ("<-12>+12", -12)
    };
    let today = now.date_naive();
    let utc_day = today.format("%Y-%m-%d").to_string();
    let next_day = (today + chrono::Days::new(1))
        .format("%Y-%m-%d")
        .to_string();
    let local_day = now
        .with_timezone(&FixedOffset::east_opt(offset_hours * 3600).unwrap())
        .format("%Y-%m-%d")
        .to_string();
    let next_day_entry = format!(
        r#"{{"timestamp":"{utc_day}T00:00:00.000000Z","level":"INFO","fields":{{"message":"entry since local midnight"}}}}"#
    );

    let log_dir = root.join(format!("tests/logs_{SUFFIX}"));
    let _ = tokio::fs::remove_dir_all(&log_dir).await;
    tokio::fs::create_dir_all(&log_dir).await.unwrap();
    let mut encoder = flate2::write::GzEncoder::new(Vec::new(), flate2::Compression::default());
    writeln!(encoder, "{COMPRESSED_ENTRY}").unwrap();
    tokio::fs::write(
        log_dir.join(format!("proxy.log.{utc_day}.gz")),
        encoder.finish().unwrap(),
    )
    .await
    .unwrap();
    tokio::fs::write(
        log_dir.join(format!("proxy.log.{next_day}")),
        format!("{next_day_entry}\n"),
    )
    .await
    .unwrap();

    let metrics_path = reset_metrics_file(root, SUFFIX).await;
    let config_path = root.join(format!("tests/config_{SUFFIX}.yaml"));
    let cookbook_path = root.join(format!("tests/cookbook_{SUFFIX}.yaml"));
    tokio::fs::write(&config_path, config_yaml(&log_dir))
        .await
        .unwrap();
    tokio::fs::write(&cookbook_path, COOKBOOK).await.unwrap();

    let mut proxy_process = tokio::process::Command::new(llamesh_binary(root))
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

    let mut files: Vec<String> = std::fs::read_dir(&log_dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().into_string().unwrap())
        .collect();
    files.sort();
    let contents =
        std::fs::read_to_string(log_dir.join(format!("proxy.log.{utc_day}"))).unwrap_or_default();

    for path in [&config_path, &cookbook_path, &metrics_path] {
        let _ = tokio::fs::remove_file(path).await;
    }
    let _ = tokio::fs::remove_dir_all(&log_dir).await;

    Run {
        utc_day,
        local_day,
        next_day_entry,
        ready,
        files,
        contents,
    }
}

#[tokio::test]
async fn log_files_follow_utc_days() {
    let root = std::env::current_dir().unwrap();
    let mut run = run_node(&root).await;
    if Utc::now().format("%Y-%m-%d").to_string() != run.utc_day {
        // The run crossed UTC midnight, so the node rightly moved on to the
        // next day's file; repeat it within a single day.
        run = run_node(&root).await;
    }

    assert!(run.ready, "proxy failed to become ready");
    assert_ne!(
        run.utc_day, run.local_day,
        "the chosen offset must change the date"
    );
    assert_eq!(
        run.files,
        [format!("proxy.log.{}", run.utc_day)],
        "expected only the UTC day's file (local day: {})",
        run.local_day
    );
    let mut lines = run.contents.lines();
    assert_eq!(
        lines.next(),
        Some(COMPRESSED_ENTRY),
        "the rotated file's entries must come first"
    );
    assert_eq!(
        lines.next(),
        Some(run.next_day_entry.as_str()),
        "the entries from the next day's file must follow"
    );
    let started: serde_json::Value = lines
        .find(|line| line.contains("Starting llamesh"))
        .map(|line| serde_json::from_str(line).unwrap())
        .expect("the restarted node's entries must follow");
    assert!(
        started["timestamp"]
            .as_str()
            .unwrap()
            .starts_with(&run.utc_day),
        "entry timestamps must fall on the file's day: {started}"
    );
}
