use reqwest::{StatusCode, Version};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

mod common;
use common::{
    cleanup_by_port_range_pattern, cleanup_procs, graceful_stop, llamesh_binary, setup_mock_script,
    wait_for_ready,
};

/// The HTTP/2 client connection preface.
const H2_PREFACE: &[u8] = b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n";

/// HTTP/2 frame types used by the raw-socket tests.
const H2_FRAME_SETTINGS: u8 = 0x4;
const H2_FRAME_PING: u8 = 0x6;
const H2_FRAME_GOAWAY: u8 = 0x7;
const H2_FLAG_ACK: u8 = 0x1;

/// Read one HTTP/2 frame: returns (type, flags, payload), or None on EOF/error.
async fn read_h2_frame(stream: &mut TcpStream) -> Option<(u8, u8, Vec<u8>)> {
    let mut head = [0u8; 9];
    stream.read_exact(&mut head).await.ok()?;
    let len = u32::from_be_bytes([0, head[0], head[1], head[2]]) as usize;
    let mut payload = vec![0u8; len];
    stream.read_exact(&mut payload).await.ok()?;
    Some((head[3], head[4], payload))
}

/// Write one HTTP/2 frame with stream id 0.
async fn write_h2_frame(stream: &mut TcpStream, frame_type: u8, flags: u8, payload: &[u8]) {
    let len = (payload.len() as u32).to_be_bytes();
    let head = [len[1], len[2], len[3], frame_type, flags, 0, 0, 0, 0];
    stream.write_all(&head).await.unwrap();
    stream.write_all(payload).await.unwrap();
}

/// Write a minimal node config for the raw-socket watchdog tests. The
/// configured `idle_timeout_seconds: 1` is floored to 30s by the server.
async fn write_watchdog_fixtures(
    root: &std::path::Path,
    suffix: &str,
    listen_port: u16,
    backend_port_start: u16,
) -> (std::path::PathBuf, std::path::PathBuf, std::path::PathBuf) {
    let mock_script = setup_mock_script(root, suffix).await;
    let config_path = root.join(format!("tests/config_{suffix}.yaml"));
    let cookbook_path = root.join(format!("tests/cookbook_{suffix}.yaml"));

    let config_content = format!(
        r#"
node_id: "test-node-{suffix}"
listen_addr: "127.0.0.1:{listen_port}"
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
    - start: {backend_port_start}
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
  idle_timeout_seconds: 1
"#,
        backend_port_start + 9,
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

    (config_path, cookbook_path, mock_script)
}

/// An h2c connection that completes the HTTP/2 handshake, answers keep-alive
/// PINGs, and then sits idle must be shut down by the idle watchdog (GOAWAY
/// followed by connection close) — ping liveness alone must not keep it open
/// forever.
#[tokio::test]
async fn test_h2c_idle_connection_closed_by_watchdog() {
    cleanup_procs("config_h2c_idle.yaml").await;
    let root = std::env::current_dir().unwrap();
    let (config_path, cookbook_path, mock_script) =
        write_watchdog_fixtures(&root, "h2c_idle", 9203, 13220).await;
    let proxy_bin = llamesh_binary(&root);

    let mut proxy_process = tokio::process::Command::new(&proxy_bin)
        .arg("--config")
        .arg(&config_path)
        .arg("--cookbook")
        .arg(&cookbook_path)
        .kill_on_drop(true)
        .spawn()
        .expect("Failed to start proxy");

    assert!(
        wait_for_ready("http://127.0.0.1:9203").await,
        "Proxy failed to become ready"
    );

    let mut stream = TcpStream::connect("127.0.0.1:9203").await.unwrap();
    stream.write_all(H2_PREFACE).await.unwrap();
    // Empty client SETTINGS completes our half of the handshake.
    write_h2_frame(&mut stream, H2_FRAME_SETTINGS, 0, &[]).await;

    let start = Instant::now();
    let deadline = Duration::from_secs(120);
    let mut saw_goaway = false;
    let closed = loop {
        let remaining = deadline.saturating_sub(start.elapsed());
        if remaining.is_zero() {
            break false;
        }
        match tokio::time::timeout(remaining, read_h2_frame(&mut stream)).await {
            Err(_) => break false, // deadline elapsed while connection stayed open
            Ok(None) => break true,
            Ok(Some((frame_type, flags, payload))) => match frame_type {
                H2_FRAME_SETTINGS if flags & H2_FLAG_ACK == 0 => {
                    write_h2_frame(&mut stream, H2_FRAME_SETTINGS, H2_FLAG_ACK, &[]).await;
                }
                // Stay "alive" from keep-alive's perspective: ack every PING.
                H2_FRAME_PING if flags & H2_FLAG_ACK == 0 => {
                    write_h2_frame(&mut stream, H2_FRAME_PING, H2_FLAG_ACK, &payload).await;
                }
                H2_FRAME_GOAWAY => saw_goaway = true,
                _ => {}
            },
        }
    };

    let elapsed = start.elapsed();
    assert!(
        closed,
        "idle h2c connection was not closed within {deadline:?}"
    );
    assert!(saw_goaway, "expected GOAWAY before close");
    assert!(
        elapsed >= Duration::from_secs(20),
        "connection closed suspiciously early ({elapsed:?}); idle floor is 30s"
    );

    graceful_stop(&mut proxy_process).await;
    let _ = tokio::fs::remove_file(&config_path).await;
    let _ = tokio::fs::remove_file(&cookbook_path).await;
    let _ = tokio::fs::remove_file(&mock_script).await;
}

/// A client that sends only the first bytes of the HTTP/2 preface and then
/// stalls never completes hyper's handshake, so it cannot observe a GOAWAY.
/// The watchdog must drop the connection outright after the idle period plus
/// the shutdown grace. On the previous implementation this connection was held
/// open indefinitely.
#[tokio::test]
async fn test_h2c_stalled_preface_closed_by_watchdog() {
    cleanup_procs("config_h2c_stall.yaml").await;
    let root = std::env::current_dir().unwrap();
    let (config_path, cookbook_path, mock_script) =
        write_watchdog_fixtures(&root, "h2c_stall", 9204, 13230).await;
    let proxy_bin = llamesh_binary(&root);

    let mut proxy_process = tokio::process::Command::new(&proxy_bin)
        .arg("--config")
        .arg(&config_path)
        .arg("--cookbook")
        .arg(&cookbook_path)
        .kill_on_drop(true)
        .spawn()
        .expect("Failed to start proxy");

    assert!(
        wait_for_ready("http://127.0.0.1:9204").await,
        "Proxy failed to become ready"
    );

    let mut stream = TcpStream::connect("127.0.0.1:9204").await.unwrap();
    // Just enough for protocol detection to classify Http2, then stall.
    stream.write_all(b"PRI ").await.unwrap();

    let start = Instant::now();
    let mut buf = [0u8; 1024];
    let closed = loop {
        let remaining = Duration::from_secs(120).saturating_sub(start.elapsed());
        if remaining.is_zero() {
            break false;
        }
        match tokio::time::timeout(remaining, stream.read(&mut buf)).await {
            Err(_) => break false,
            Ok(Ok(0)) | Ok(Err(_)) => break true,
            Ok(Ok(_)) => {} // server SETTINGS etc.; keep reading until EOF
        }
    };

    assert!(
        closed,
        "stalled h2c preface held the connection open past the watchdog bound"
    );

    graceful_stop(&mut proxy_process).await;
    let _ = tokio::fs::remove_file(&config_path).await;
    let _ = tokio::fs::remove_file(&cookbook_path).await;
    let _ = tokio::fs::remove_file(&mock_script).await;
}

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
