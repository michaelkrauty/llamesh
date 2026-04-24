#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::time::Duration;
use tokio::time::sleep;

pub fn llamesh_binary(root: &Path) -> PathBuf {
    if let Some(path) = std::env::var_os("CARGO_BIN_EXE_llamesh") {
        return PathBuf::from(path);
    }

    let debug_bin = root.join("target/debug/llamesh");
    if debug_bin.exists() {
        debug_bin
    } else {
        root.join("target/release/llamesh")
    }
}

pub async fn setup_mock_script(root: &Path, suffix: &str) -> PathBuf {
    let mock_script = root.join(format!("tests/mock_server_{suffix}.sh"));
    let mock_bin = std::env::var_os("CARGO_BIN_EXE_mock_llama_server")
        .map(PathBuf::from)
        .unwrap_or_else(|| {
            let debug_bin = root.join("target/debug/mock_llama_server");
            if debug_bin.exists() {
                debug_bin
            } else {
                root.join("target/release/mock_llama_server")
            }
        });
    let script_content = format!(
        r#"#!/bin/bash
BIN="{}"
"$BIN" "$@" &
PID=$!
trap "kill $PID" EXIT TERM INT
wait $PID
"#,
        mock_bin.display()
    );
    tokio::fs::write(&mock_script, script_content)
        .await
        .unwrap();

    use std::os::unix::fs::PermissionsExt;
    let mut perms = tokio::fs::metadata(&mock_script)
        .await
        .unwrap()
        .permissions();
    perms.set_mode(0o755);
    tokio::fs::set_permissions(&mock_script, perms)
        .await
        .unwrap();
    mock_script
}

pub async fn cleanup_procs(pattern: &str) {
    let _ = tokio::process::Command::new("pkill")
        .arg("-f")
        .arg(pattern)
        .output()
        .await;
}

pub async fn cleanup_by_port_range_pattern(port_pattern: &str) {
    // Kill mock_llama_server processes that match the port pattern
    // e.g. port_pattern = "1300[0-9]"
    // match "--port 1300[0-9]"
    let full_pattern = format!("mock_llama_server.*--port {port_pattern}");
    let _ = tokio::process::Command::new("pkill")
        .arg("-f")
        .arg(&full_pattern)
        .output()
        .await;
}

pub async fn graceful_stop(child: &mut tokio::process::Child) {
    if let Some(pid) = child.id() {
        let _ = tokio::process::Command::new("kill")
            .arg(pid.to_string())
            .output()
            .await;

        // Wait for it to exit
        if (tokio::time::timeout(Duration::from_secs(5), child.wait()).await).is_err() {
            let _ = child.kill().await;
        }
    }
}

pub async fn wait_for_ready(base_url: &str) -> bool {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(1))
        .build()
        .unwrap();

    for _ in 0..60 {
        if let Ok(resp) = client.get(format!("{base_url}/healthz")).send().await {
            if resp.status().is_success() {
                return true;
            }
        }
        sleep(Duration::from_millis(500)).await;
    }
    false
}

// =============================================================================
// Test fixtures helpers
// =============================================================================

/// Configuration parameters for test config generation
pub struct TestConfigParams<'a> {
    pub node_id: &'a str,
    pub listen_addr: &'a str,
    pub port_start: u16,
    pub port_end: u16,
    pub mock_binary: &'a Path,
    pub default_model: &'a str,
}

/// Load config template and substitute placeholders
#[allow(dead_code)]
pub async fn load_config_from_template(params: &TestConfigParams<'_>) -> String {
    let template = tokio::fs::read_to_string("tests/fixtures/config_template.yaml")
        .await
        .expect("Failed to read config template");

    template
        .replace("{{NODE_ID}}", params.node_id)
        .replace("{{LISTEN_ADDR}}", params.listen_addr)
        .replace("{{PORT_START}}", &params.port_start.to_string())
        .replace("{{PORT_END}}", &params.port_end.to_string())
        .replace("{{MOCK_BINARY}}", &params.mock_binary.display().to_string())
        .replace("{{DEFAULT_MODEL}}", params.default_model)
}

/// Load a cookbook fixture by name (without .yaml extension)
#[allow(dead_code)]
pub async fn load_cookbook_fixture(name: &str) -> String {
    tokio::fs::read_to_string(format!("tests/fixtures/{name}.yaml"))
        .await
        .unwrap_or_else(|_| panic!("Failed to read cookbook fixture: {name}"))
}

/// Find an available port in the given range
#[allow(dead_code)]
pub async fn find_available_port(start: u16, end: u16) -> Option<u16> {
    for port in start..=end {
        if tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .is_ok()
        {
            return Some(port);
        }
    }
    None
}
