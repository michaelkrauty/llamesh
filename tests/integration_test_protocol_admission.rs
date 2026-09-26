#![cfg(target_os = "linux")]

use std::{net::SocketAddr, process::Child, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::{sleep, timeout},
};

const FAST: Duration = Duration::from_secs(2);
const DETECT_MS: u64 = 10_000;
const BODY_MS: u64 = 500;

struct Server {
    child: Child,
    addr: SocketAddr,
    _dir: tempfile::TempDir,
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Server {
    async fn start(detect_ms: u64) -> Self {
        Self::with_body_limit(detect_ms, 1048576).await
    }

    async fn with_body_limit(detect_ms: u64, body_limit: usize) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        std::fs::write(
            dir.path().join("config.yaml"),
            format!(
                r#"node_id: protocol-admission-test
listen_addr: "{addr}"
metrics_path: metrics.json
max_vram_mb: 1024
max_sysmem_mb: 1024
default_model: unused
shutdown_grace_period_seconds: 1
model_defaults:
  max_concurrent_requests_per_instance: 1
  max_queue_size_per_model: 1
  max_instances_per_model: 1
  max_wait_in_queue_ms: 100
llama_cpp_ports:
  ranges: [{{start: 30000, end: 30001}}]
llama_cpp:
  enabled: false
  repo_url: ""
  repo_path: .
  build_path: .
  binary_path: /nonexistent
  branch: master
  build_args: []
  build_command_args: []
  auto_update_interval_seconds: 0
cluster:
  enabled: false
  peers: []
  gossip_interval_seconds: 5
  discovery: {{mdns: false}}
  noise: {{enabled: false}}
http:
  request_body_limit_bytes: {body_limit}
  idle_timeout_seconds: 60
  protocol_detect_timeout_ms: {detect_ms}
  body_read_timeout_ms: {BODY_MS}
"#
            ),
        )
        .unwrap();
        std::fs::write(dir.path().join("cookbook.yaml"), "models: []\n").unwrap();
        let binary = std::env::var_os("LLAMESH_PROTOCOL_ADMISSION_BINARY")
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_llamesh").into());
        drop(listener);
        let child = std::process::Command::new(binary)
            .args(["--config", "config.yaml", "--cookbook", "cookbook.yaml"])
            .current_dir(dir.path())
            .env_clear()
            .env("HOME", dir.path())
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::inherit())
            .spawn()
            .unwrap();
        let mut server = Self {
            child,
            addr,
            _dir: dir,
        };
        let client = body_client(false);
        let mut last_error = None;
        timeout(Duration::from_secs(10), async {
            loop {
                assert!(
                    server.child.try_wait().unwrap().is_none(),
                    "server at {addr} exited during startup: {last_error:?}"
                );
                // A successful TCP connect alone does not establish readiness.
                // Retry only startup transport failures, within this bound;
                // requests made by the tests below retain strict assertions.
                match client.get(format!("http://{addr}/version")).send().await {
                    Ok(response) => {
                        assert_eq!(response.status(), reqwest::StatusCode::OK);
                        match response.json::<serde_json::Value>().await {
                            Ok(body) => {
                                assert!(body["version"].is_string(), "{addr}: {body}");
                                break;
                            }
                            Err(error) if error.is_decode() => {
                                panic!("invalid startup response from {addr}: {error}")
                            }
                            Err(error) => last_error = Some(error),
                        }
                    }
                    Err(error) => last_error = Some(error),
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .unwrap_or_else(|_| panic!("server startup at {addr} timed out: {last_error:?}"));
        server
    }

    async fn stalled(&self, prefix: &[u8]) -> TcpStream {
        let mut stream = timeout(FAST, TcpStream::connect(self.addr))
            .await
            .unwrap()
            .unwrap();
        stream.write_all(prefix).await.unwrap();
        // Observe this exact accepted socket in the child's fd table. A TCP
        // handshake alone could still leave it queued when SIGTERM arrives.
        let peer_port = stream.local_addr().unwrap().port();
        timeout(FAST, async {
            loop {
                let tcp =
                    std::fs::read_to_string(format!("/proc/{}/net/tcp", self.child.id())).unwrap();
                let inode = tcp.lines().skip(1).find_map(|line| {
                    let fields: Vec<_> = line.split_whitespace().collect();
                    let local = fields[1].rsplit(':').next()?;
                    let remote = fields[2].rsplit(':').next()?;
                    (u16::from_str_radix(local, 16).ok()? == self.addr.port()
                        && u16::from_str_radix(remote, 16).ok()? == peer_port)
                        .then(|| format!("socket:[{}]", fields[9]))
                });
                if let Some(inode) = inode {
                    let accepted = std::fs::read_dir(format!("/proc/{}/fd", self.child.id()))
                        .unwrap()
                        .filter_map(Result::ok)
                        .filter_map(|entry| std::fs::read_link(entry.path()).ok())
                        .any(|path| path.to_string_lossy() == inode);
                    if accepted {
                        break;
                    }
                }
                sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .expect("socket was not accepted");
        stream
    }

    async fn version(&self, h2: bool) {
        let builder = reqwest::Client::builder()
            .no_proxy()
            .timeout(FAST)
            .pool_max_idle_per_host(0);
        let client = if h2 {
            builder.http2_prior_knowledge()
        } else {
            builder.http1_only()
        }
        .build()
        .unwrap();
        let response = client
            .get(format!("http://{}/version", self.addr))
            .send()
            .await
            .expect("independent /version blocked by incomplete protocol detection");
        assert_eq!(response.status(), reqwest::StatusCode::OK);
        assert_eq!(
            response.version(),
            if h2 {
                reqwest::Version::HTTP_2
            } else {
                reqwest::Version::HTTP_11
            }
        );
        let body: serde_json::Value = response.json().await.unwrap();
        assert!(body["version"].is_string());
    }
}

fn body_client(h2: bool) -> reqwest::Client {
    let builder = reqwest::Client::builder().no_proxy().timeout(FAST);
    if h2 {
        builder.http2_prior_knowledge()
    } else {
        builder.http1_only()
    }
    .build()
    .unwrap()
}

async fn json_body(server: &Server, path: &str) -> String {
    if path == "/admin/prewarm" {
        return r#"{"model":"missing"}"#.into();
    }
    let nodes: serde_json::Value = body_client(false)
        .get(format!("http://{}/cluster/nodes", server.addr))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut origin = nodes["nodes"]["protocol-admission-test"].clone();
    origin["node_id"] = "body-deadline-peer".into();
    serde_json::json!({"origin": origin, "known_peers": []}).to_string()
}

async fn body_deadline(h2: bool, path: &str) {
    let server = Server::start(DETECT_MS).await;
    let json = json_body(&server, path).await;
    let partial = bytes::Bytes::copy_from_slice(&json.as_bytes()[..json.len() - 1]);
    // Emit real DATA (or an HTTP/1 chunk), then leave the request body open.
    let body = futures::stream::once(async { Ok::<_, std::io::Error>(partial) });
    let body = futures::StreamExt::chain(body, futures::stream::pending());
    let started = std::time::Instant::now();
    let response = body_client(h2)
        .post(format!("http://{}{path}", server.addr))
        .header("content-type", "application/json")
        .body(reqwest::Body::wrap_stream(body))
        .send()
        .await
        .expect("JSON body deadline did not produce a response");
    assert_eq!(response.status(), reqwest::StatusCode::REQUEST_TIMEOUT);
    assert_eq!(
        response.version(),
        if h2 {
            reqwest::Version::HTTP_2
        } else {
            reqwest::Version::HTTP_11
        }
    );
    assert!(started.elapsed() >= Duration::from_millis(BODY_MS / 2));
    let error: serde_json::Value = response.json().await.unwrap();
    assert_eq!(error["error"]["type"], "request_timeout");

    let nodes: serde_json::Value = body_client(h2)
        .get(format!("http://{}/cluster/nodes", server.addr))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert!(
        nodes["nodes"].get("body-deadline-peer").is_none(),
        "incomplete gossip mutated peer state"
    );
}

#[tokio::test]
async fn http1_prewarm_body_deadline() {
    body_deadline(false, "/admin/prewarm").await;
}

#[tokio::test]
async fn h2c_prewarm_body_deadline() {
    body_deadline(true, "/admin/prewarm").await;
}

#[tokio::test]
async fn http1_gossip_body_deadline() {
    body_deadline(false, "/cluster/gossip").await;
}

#[tokio::test]
async fn h2c_gossip_body_deadline() {
    body_deadline(true, "/cluster/gossip").await;
}

#[tokio::test]
async fn json_body_completion_and_rejections() {
    for h2 in [false, true] {
        let server = Server::start(DETECT_MS).await;
        let client = body_client(h2);
        for path in ["/admin/prewarm", "/cluster/gossip"] {
            let json = json_body(&server, path).await;
            for fragmented in [false, true] {
                let body = if fragmented {
                    let split = json.len() / 2;
                    let first = bytes::Bytes::copy_from_slice(&json.as_bytes()[..split]);
                    let second = bytes::Bytes::copy_from_slice(&json.as_bytes()[split..]);
                    let chunks = futures::StreamExt::chain(
                        futures::stream::once(async { Ok::<_, std::io::Error>(first) }),
                        futures::stream::once(async {
                            sleep(Duration::from_millis(20)).await;
                            Ok::<_, std::io::Error>(second)
                        }),
                    );
                    reqwest::Body::wrap_stream(chunks)
                } else {
                    reqwest::Body::from(json.clone())
                };
                let response = client
                    .post(format!("http://{}{path}", server.addr))
                    .header("content-type", "application/json")
                    .body(body)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(
                    response.status().as_u16(),
                    if path == "/admin/prewarm" { 404 } else { 200 }
                );
            }
            for (content_type, body, expected) in
                [("text/plain", "{}", 415), ("application/json", "{", 400)]
            {
                let response = client
                    .post(format!("http://{}{path}", server.addr))
                    .header("content-type", content_type)
                    .body(body)
                    .send()
                    .await
                    .unwrap();
                assert_eq!(response.status().as_u16(), expected);
            }
        }
        let response = client
            .post(format!("http://{}/cluster/gossip", server.addr))
            .json(&serde_json::json!({}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status().as_u16(), 422);
    }
}

async fn json_body_limit(h2: bool, path: &str, raised: bool) {
    let limit = if raised { 2 * 1024 * 1024 + 1024 } else { 4096 };
    let server = Server::with_body_limit(DETECT_MS, limit).await;
    let client = body_client(h2);
    let template = json_body(&server, path).await;
    // Reject first, using distinct peer IDs so a previously accepted gossip
    // cannot hide an erroneous state mutation by an oversized request.
    for streamed in [false, true] {
        for size in [limit + 1, limit, limit - 1] {
            let peer = format!("size-peer-{streamed}-{size}");
            let mut value: serde_json::Value = serde_json::from_str(&template).unwrap();
            if path == "/cluster/gossip" {
                value["origin"]["node_id"] = peer.clone().into();
            }
            let mut json = serde_json::to_vec(&value).unwrap();
            assert!(json.len() < size);
            // JSON whitespace counts toward the byte limit without changing
            // handler semantics or requiring a huge parsed string allocation.
            json.resize(size, b' ');
            let body = if streamed {
                let bytes = bytes::Bytes::from(json);
                let chunks: Vec<_> = (0..bytes.len())
                    .step_by(1024)
                    .map(|start| {
                        Ok::<_, std::io::Error>(bytes.slice(start..(start + 1024).min(bytes.len())))
                    })
                    .collect();
                // wrap_stream has no exact byte size: HTTP/1 uses chunked
                // framing, HTTP/2 DATA arrives without Content-Length.
                reqwest::Body::wrap_stream(futures::stream::iter(chunks))
            } else {
                reqwest::Body::from(json)
            };
            let request = client
                .post(format!("http://{}{path}", server.addr))
                .header("content-type", "application/json")
                .body(body)
                .build()
                .unwrap();
            if streamed {
                assert!(request.headers().get("content-length").is_none());
            }
            let response = client.execute(request).await.unwrap();
            assert_eq!(
                response.version(),
                if h2 {
                    reqwest::Version::HTTP_2
                } else {
                    reqwest::Version::HTTP_11
                }
            );
            let expected = if size > limit {
                413
            } else if path == "/admin/prewarm" {
                404
            } else {
                200
            };
            let status = response.status().as_u16();
            let response_body = response.text().await.unwrap();
            if path == "/cluster/gossip" {
                let nodes: serde_json::Value = client
                    .get(format!("http://{}/cluster/nodes", server.addr))
                    .send()
                    .await
                    .unwrap()
                    .json()
                    .await
                    .unwrap();
                assert_eq!(nodes["nodes"].get(&peer).is_some(), size <= limit,
                    "gossip state disagrees with admission: size={size}, limit={limit}, streamed={streamed}, status={status}");
            }
            assert_eq!(
                status, expected,
                "{path}: size={size}, limit={limit}, streamed={streamed}, response={response_body}"
            );
        }
    }
}

#[tokio::test]
async fn http1_prewarm_small_body_limit() {
    json_body_limit(false, "/admin/prewarm", false).await;
}

#[tokio::test]
async fn h2c_prewarm_small_body_limit() {
    json_body_limit(true, "/admin/prewarm", false).await;
}

#[tokio::test]
async fn http1_gossip_small_body_limit() {
    json_body_limit(false, "/cluster/gossip", false).await;
}

#[tokio::test]
async fn h2c_gossip_small_body_limit() {
    json_body_limit(true, "/cluster/gossip", false).await;
}

#[tokio::test]
async fn http1_prewarm_raised_body_limit() {
    json_body_limit(false, "/admin/prewarm", true).await;
}

#[tokio::test]
async fn h2c_prewarm_raised_body_limit() {
    json_body_limit(true, "/admin/prewarm", true).await;
}

#[tokio::test]
async fn http1_gossip_raised_body_limit() {
    json_body_limit(false, "/cluster/gossip", true).await;
}

#[tokio::test]
async fn h2c_gossip_raised_body_limit() {
    json_body_limit(true, "/cluster/gossip", true).await;
}

async fn version_on_socket(stream: &mut TcpStream, suffix: &[u8]) {
    timeout(FAST, async {
        stream.write_all(suffix).await.unwrap();
        let mut response = Vec::new();
        stream.read_to_end(&mut response).await.unwrap();
        let expected = if suffix.starts_with(b"OST ") {
            b"HTTP/1.1 405"
        } else {
            b"HTTP/1.1 200"
        };
        assert!(
            response.starts_with(expected),
            "unexpected response: {}",
            String::from_utf8_lossy(&response)
        );
    })
    .await
    .expect("delayed request did not complete");
}

#[tokio::test]
async fn incomplete_detection_does_not_block_http1_or_h2c() {
    for prefix in [b"".as_slice(), b"\x16", b"P", b"PRI"] {
        let server = Server::start(DETECT_MS).await;
        let _stalled = server.stalled(prefix).await;
        server.version(false).await;
        server.version(true).await;
    }
}

#[tokio::test]
async fn delayed_request_preserves_detection_prefix() {
    let server = Server::start(DETECT_MS).await;
    let mut silent = server.stalled(b"").await;
    server.version(false).await;
    version_on_socket(
        &mut silent,
        b"GET /version HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
    )
    .await;
    let mut delayed = server.stalled(b"P").await;
    // Completing an independent request is the barrier before releasing the
    // first connection, rather than a scheduling-dependent sleep.
    server.version(false).await;
    version_on_socket(&mut delayed, b"OST /version HTTP/1.1\r\nHost: localhost\r\nContent-Length: 0\r\nConnection: close\r\n\r\n").await;
}

#[tokio::test]
async fn silent_detection_times_out() {
    let server = Server::start(300).await;
    let started = std::time::Instant::now();
    let mut stalled = server.stalled(b"").await;
    let mut byte = [0];
    assert_eq!(
        timeout(FAST, stalled.read(&mut byte))
            .await
            .expect("detection deadline did not close socket")
            .unwrap(),
        0
    );
    assert!(
        started.elapsed() >= Duration::from_millis(200),
        "silent connection closed before the detection deadline"
    );
    server.version(false).await;
}

#[tokio::test]
async fn sigterm_exits_with_pending_protocol_detection() {
    let mut server = Server::start(DETECT_MS).await;
    let _stalled = server.stalled(b"").await;
    nix::sys::signal::kill(
        nix::unistd::Pid::from_raw(server.child.id() as i32),
        nix::sys::signal::Signal::SIGTERM,
    )
    .unwrap();
    timeout(FAST, async {
        loop {
            if let Some(status) = server.child.try_wait().unwrap() {
                assert!(status.success(), "server did not exit gracefully: {status}");
                break;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("SIGTERM was blocked by pending protocol detection");
}
