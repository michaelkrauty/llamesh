//! Hop-by-hop header fields (RFC 9110 §7.6.1) describe a single connection and
//! must not cross the proxy in either direction.
//!
//! The mock server is configured via `MOCK_CONNECTION_CLOSE=1` to end every
//! connection with `Connection: close`, as `llama-server` does on the last
//! response of a keep-alive connection. Relaying that field would make the
//! proxy close the client's connection as well, so the test drives several
//! requests over one raw client connection and requires each to be answered.
//! `MOCK_ECHO_REQUEST_HEADERS=1` reports the request header names the mock
//! received, showing which client fields were forwarded.

use std::time::Duration;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

mod common;
use common::{
    cleanup_by_port_range_pattern, cleanup_procs, graceful_stop, llamesh_binary,
    setup_mock_script_with_env, wait_for_ready,
};

const LISTEN_ADDR: &str = "127.0.0.1:9241";
const BASE_URL: &str = "http://127.0.0.1:9241";
const SUFFIX: &str = "hop_headers";

/// Header fields that must never be relayed, including one the client marks
/// as hop-by-hop by naming it in `Connection`.
const HOP_BY_HOP: [&str; 9] = [
    "connection",
    "keep-alive",
    "proxy-authenticate",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "upgrade",
    "x-hop-scoped",
];

/// Client request fields that are hop-by-hop, plus an end-to-end field that
/// must still be forwarded.
const CLIENT_HEADERS: &str = "Connection: keep-alive, X-Hop-Scoped\r\n\
    X-Hop-Scoped: 1\r\n\
    Keep-Alive: timeout=30\r\n\
    Proxy-Authorization: Basic eA==\r\n\
    Proxy-Connection: keep-alive\r\n\
    TE: trailers\r\n\
    X-End-To-End: kept\r\n";

fn config_yaml(mock_script: &std::path::Path) -> String {
    format!(
        r#"
node_id: "test-{SUFFIX}"
listen_addr: "{LISTEN_ADDR}"
metrics_path: "./tests/metrics_{SUFFIX}.json"
max_vram_mb: 1048576
max_sysmem_mb: 1024
default_model: "mock-model:default"
model_defaults:
  max_concurrent_requests_per_instance: 2
  max_queue_size_per_model: 10
  max_instances_per_model: 1
  max_wait_in_queue_ms: 5000
llama_cpp_ports:
  ranges:
    - start: 13270
      end: 13279
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
      - id: "default"
        model_path: "./models/mock.gguf"
        idle_timeout_seconds: 60
        max_instances: 1
        llama_server_args: ""
"#;

struct RawResponse {
    status: u16,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl RawResponse {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
    }
}

fn chat_request(stream: bool) -> Vec<u8> {
    let body = format!(
        r#"{{"model":"mock-model:default","messages":[{{"role":"user","content":"hi"}}],"stream":{stream}}}"#
    );
    format!(
        "POST /v1/chat/completions HTTP/1.1\r\n\
         Host: {LISTEN_ADDR}\r\n\
         Content-Type: application/json\r\n\
         Content-Length: {}\r\n\
         {CLIENT_HEADERS}\r\n\
         {body}",
        body.len()
    )
    .into_bytes()
}

/// Read one HTTP/1.1 response, or `None` if the connection closes first.
async fn read_response(reader: &mut BufReader<TcpStream>) -> Option<RawResponse> {
    let mut line = String::new();
    if reader.read_line(&mut line).await.ok()? == 0 {
        return None;
    }
    let status = line.split_whitespace().nth(1)?.parse().ok()?;

    let mut headers = Vec::new();
    loop {
        line.clear();
        if reader.read_line(&mut line).await.ok()? == 0 {
            return None;
        }
        let field = line.trim_end();
        if field.is_empty() {
            break;
        }
        let (name, value) = field.split_once(':')?;
        headers.push((name.trim().to_ascii_lowercase(), value.trim().to_string()));
    }
    let mut response = RawResponse {
        status,
        headers,
        body: Vec::new(),
    };

    if let Some(length) = response.header("content-length") {
        let mut body = vec![0; length.parse().ok()?];
        reader.read_exact(&mut body).await.ok()?;
        response.body = body;
    } else if response.header("transfer-encoding") == Some("chunked") {
        loop {
            line.clear();
            reader.read_line(&mut line).await.ok()?;
            let size = usize::from_str_radix(line.trim().split(';').next()?, 16).ok()?;
            // Chunk data followed by its CRLF; the last chunk is just the CRLF
            // that ends the (trailer-less) message.
            let mut chunk = vec![0; size + 2];
            reader.read_exact(&mut chunk).await.ok()?;
            if size == 0 {
                break;
            }
            response.body.extend_from_slice(&chunk[..size]);
        }
    }
    Some(response)
}

/// Send `request` on the client connection and read its response.
async fn exchange(reader: &mut BufReader<TcpStream>, request: &[u8]) -> Option<RawResponse> {
    reader.get_mut().write_all(request).await.ok()?;
    tokio::time::timeout(Duration::from_secs(30), read_response(reader))
        .await
        .ok()?
}

fn assert_relayed_cleanly(response: &RawResponse, label: &str) {
    assert_eq!(response.status, 200, "{label}: unexpected status");
    for name in ["connection", "keep-alive"] {
        assert!(
            response.header(name).is_none(),
            "{label}: upstream `{name}` relayed to the client"
        );
    }

    let forwarded: Vec<&str> = response
        .header("x-mock-request-headers")
        .unwrap_or_else(|| panic!("{label}: mock did not report received headers"))
        .split(',')
        .collect();
    assert!(
        forwarded.contains(&"x-end-to-end"),
        "{label}: end-to-end client header was not forwarded: {forwarded:?}"
    );
    for name in HOP_BY_HOP {
        assert!(
            !forwarded.contains(&name),
            "{label}: client `{name}` forwarded to the instance: {forwarded:?}"
        );
    }
}

#[tokio::test]
async fn hop_by_hop_headers_are_not_relayed() {
    cleanup_procs("mock_server_hop_headers.sh").await;
    cleanup_procs("config_hop_headers.yaml").await;
    cleanup_by_port_range_pattern("1327[0-9]").await;

    let root = std::env::current_dir().unwrap();
    let mock_script = setup_mock_script_with_env(
        &root,
        SUFFIX,
        &[
            ("MOCK_CONNECTION_CLOSE", "1"),
            ("MOCK_ECHO_REQUEST_HEADERS", "1"),
        ],
    )
    .await;
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

    let mut client = BufReader::new(TcpStream::connect(LISTEN_ADDR).await.unwrap());

    // Every upstream response ends its upstream connection, yet each request
    // must be answered on the same client connection.
    for (i, stream) in [false, true, false].into_iter().enumerate() {
        let label = format!("request {} (stream: {stream})", i + 1);
        let response = exchange(&mut client, &chat_request(stream))
            .await
            .unwrap_or_else(|| panic!("{label}: proxy closed the client connection"));
        assert_relayed_cleanly(&response, &label);
        let marker: &[u8] = if stream { b"[DONE]" } else { b"\"choices\"" };
        assert!(
            response.body.windows(marker.len()).any(|w| w == marker),
            "{label}: incomplete body: {}",
            String::from_utf8_lossy(&response.body)
        );
    }

    graceful_stop(&mut proxy_process).await;
    cleanup_procs("mock_server_hop_headers.sh").await;
    cleanup_by_port_range_pattern("1327[0-9]").await;
    for path in [mock_script, config_path, cookbook_path, metrics_path] {
        let _ = tokio::fs::remove_file(path).await;
    }
}
