//! A cookbook profile with an `upstream` is served by that OpenAI-compatible
//! endpoint while its health checks pass, without starting a local instance.
//!
//! The upstream is a stub HTTP/1.1 server inside the test process, written on
//! a raw TCP listener so tests control connection-level behaviour: holding a
//! request, closing without an answer, streaming, and seeing the peer close.
//! The local fallback is the repository's mock llama-server, whose chat answer
//! contains "mock response" and so tells the two backends apart.

use futures::{FutureExt, StreamExt};
use reqwest::StatusCode;
use serde_json::{json, Value};
use std::future::Future;
use std::panic::AssertUnwindSafe;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU16, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::Notify;

mod common;
use common::{
    cleanup_by_port_range_pattern, cleanup_procs, graceful_stop, llamesh_binary,
    reset_metrics_file, setup_mock_script, wait_for_ready,
};

const MODEL: &str = "mock-model:default";
const STUB_BODY: &str = r#"{"id":"stub","object":"chat.completion","choices":[{"index":0,"message":{"role":"assistant","content":"from the stub"},"finish_reason":"stop"}]}"#;

// =============================================================================
// Stub upstream
// =============================================================================

#[derive(Clone, Debug)]
struct Recorded {
    method: String,
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Recorded {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(name))
            .map(|(_, v)| v.as_str())
    }
}

/// What the stub does with a `POST`.
#[derive(Clone)]
enum Mode {
    Respond {
        status: u16,
        body: String,
        headers: Vec<(String, String)>,
        delay: Duration,
    },
    /// Keep the request until `release`, watching for the peer to close.
    Hold,
    /// Close the connection without answering.
    Close,
    /// Answer with SSE events sent `gap` apart in chunked encoding.
    Stream { events: Vec<String>, gap: Duration },
    /// Start a chunked SSE answer, then close the connection without ending it.
    BrokenStream,
}

impl Mode {
    fn ok() -> Self {
        Self::ok_after(Duration::ZERO)
    }

    fn ok_after(delay: Duration) -> Self {
        Mode::Respond {
            status: 200,
            body: STUB_BODY.to_string(),
            headers: Vec::new(),
            delay,
        }
    }
}

struct StubState {
    health_status: AtomicU16,
    mode: Mutex<Mode>,
    requests: Mutex<Vec<Recorded>>,
    active: AtomicUsize,
    max_active: AtomicUsize,
    held: AtomicUsize,
    held_closed: AtomicUsize,
    release: Notify,
}

#[derive(Clone)]
struct Stub {
    url: String,
    state: Arc<StubState>,
}

impl Stub {
    async fn start(health_status: u16, mode: Mode) -> Stub {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let state = Arc::new(StubState {
            health_status: AtomicU16::new(health_status),
            mode: Mutex::new(mode),
            requests: Mutex::new(Vec::new()),
            active: AtomicUsize::new(0),
            max_active: AtomicUsize::new(0),
            held: AtomicUsize::new(0),
            held_closed: AtomicUsize::new(0),
            release: Notify::new(),
        });
        let accept_state = state.clone();
        tokio::spawn(async move {
            while let Ok((socket, _)) = listener.accept().await {
                tokio::spawn(serve(socket, accept_state.clone()));
            }
        });
        Stub { url, state }
    }

    fn set_health(&self, status: u16) {
        self.state.health_status.store(status, Ordering::SeqCst);
    }

    fn set_mode(&self, mode: Mode) {
        *self.state.mode.lock().unwrap() = mode;
    }

    fn posts(&self) -> Vec<Recorded> {
        self.recorded("POST")
    }

    fn recorded(&self, method: &str) -> Vec<Recorded> {
        let requests = self.state.requests.lock().unwrap();
        requests
            .iter()
            .filter(|r| r.method == method)
            .cloned()
            .collect()
    }

    fn held(&self) -> usize {
        self.state.held.load(Ordering::SeqCst)
    }

    /// Answer every held request.
    fn release(&self) {
        self.state.release.notify_waiters();
    }

    fn held_closed(&self) -> usize {
        self.state.held_closed.load(Ordering::SeqCst)
    }

    fn max_active(&self) -> usize {
        self.state.max_active.load(Ordering::SeqCst)
    }
}

/// Read one request: head up to the blank line, then `Content-Length` bytes.
async fn read_request(socket: &mut TcpStream) -> Option<Recorded> {
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let head_end = loop {
        if let Some(pos) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos;
        }
        let n = socket.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
    let mut lines = head.split("\r\n");
    let mut start = lines.next()?.split(' ');
    let method = start.next()?.to_string();
    let path = start.next()?.to_string();
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_string()))
        .collect();
    let length: usize = headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .and_then(|(_, v)| v.parse().ok())
        .unwrap_or(0);
    let mut body = buf[head_end + 4..].to_vec();
    while body.len() < length {
        let n = socket.read(&mut chunk).await.ok()?;
        if n == 0 {
            return None;
        }
        body.extend_from_slice(&chunk[..n]);
    }
    Some(Recorded {
        method,
        path,
        headers,
        body,
    })
}

fn response_head(status: u16, headers: &[(String, String)], length: Option<usize>) -> String {
    let mut head = format!("HTTP/1.1 {status} Stub\r\nConnection: close\r\n");
    for (name, value) in headers {
        head.push_str(&format!("{name}: {value}\r\n"));
    }
    match length {
        Some(length) => head.push_str(&format!("Content-Length: {length}\r\n")),
        None => head.push_str("Transfer-Encoding: chunked\r\n"),
    }
    head.push_str("\r\n");
    head
}

async fn respond(socket: &mut TcpStream, status: u16, headers: &[(String, String)], body: &str) {
    let mut all = vec![("Content-Type".to_string(), "application/json".to_string())];
    all.extend(headers.iter().cloned());
    let head = response_head(status, &all, Some(body.len()));
    let _ = socket.write_all(head.as_bytes()).await;
    let _ = socket.write_all(body.as_bytes()).await;
    let _ = socket.shutdown().await;
}

async fn serve(mut socket: TcpStream, state: Arc<StubState>) {
    let Some(request) = read_request(&mut socket).await else {
        return;
    };
    let method = request.method.clone();
    state.requests.lock().unwrap().push(request);
    if method == "GET" {
        let status = state.health_status.load(Ordering::SeqCst);
        respond(&mut socket, status, &[], r#"{"object":"list","data":[]}"#).await;
        return;
    }

    let active = state.active.fetch_add(1, Ordering::SeqCst) + 1;
    state.max_active.fetch_max(active, Ordering::SeqCst);
    let mode = state.mode.lock().unwrap().clone();
    match mode {
        Mode::Respond {
            status,
            body,
            headers,
            delay,
        } => {
            tokio::time::sleep(delay).await;
            respond(&mut socket, status, &headers, &body).await;
        }
        Mode::Hold => {
            let release = state.release.notified();
            tokio::pin!(release);
            state.held.fetch_add(1, Ordering::SeqCst);
            let mut byte = [0u8; 1];
            tokio::select! {
                _ = &mut release => respond(&mut socket, 200, &[], STUB_BODY).await,
                // Nothing more is due from the peer, so any read result
                // (end of stream or an error) means it closed the connection.
                _ = socket.read(&mut byte) => {
                    state.held_closed.fetch_add(1, Ordering::SeqCst);
                }
            }
        }
        Mode::Close => drop(socket),
        Mode::Stream { events, gap } => {
            let headers = [("Content-Type".to_string(), "text/event-stream".to_string())];
            let _ = socket
                .write_all(response_head(200, &headers, None).as_bytes())
                .await;
            for event in events {
                let data = format!("data: {event}\n\n");
                let chunk = format!("{:x}\r\n{data}\r\n", data.len());
                let _ = socket.write_all(chunk.as_bytes()).await;
                let _ = socket.flush().await;
                tokio::time::sleep(gap).await;
            }
            let _ = socket.write_all(b"0\r\n\r\n").await;
            let _ = socket.shutdown().await;
        }
        Mode::BrokenStream => {
            let headers = [("Content-Type".to_string(), "text/event-stream".to_string())];
            let data = "data: {\"choices\":[{\"delta\":{\"content\":\"part0\"}}]}\n\n";
            let head = response_head(200, &headers, None);
            let chunk = format!("{:x}\r\n{data}\r\n", data.len());
            let _ = socket.write_all(head.as_bytes()).await;
            let _ = socket.write_all(chunk.as_bytes()).await;
            let _ = socket.flush().await;
            // Long enough for the chunk to be relayed before the stream breaks.
            tokio::time::sleep(Duration::from_millis(300)).await;
        }
    }
    state.active.fetch_sub(1, Ordering::SeqCst);
}

// =============================================================================
// llamesh node
// =============================================================================

struct Spec {
    suffix: &'static str,
    listen_addr: &'static str,
    port_start: u16,
    port_pattern: &'static str,
    auth_key: Option<&'static str>,
}

/// What a test body gets: the node's URL and process, a client and the
/// cookbook path.
#[derive(Clone)]
struct Ctx {
    base_url: String,
    node_id: String,
    pid: u32,
    client: reqwest::Client,
    cookbook_path: PathBuf,
}

/// Health checks fast enough that health changes take well under a second.
fn upstream_yaml(url: &str, extra: &str) -> String {
    format!(
        r#"        upstream:
          url: "{url}"
          max_concurrent_requests: 1
{extra}          health_check:
            path: "/health"
            interval_ms: 200
            timeout_ms: 500
            failure_threshold: 2
"#
    )
}

/// A cookbook with one profile; `local` gives it a model source to fall back to.
fn cookbook(local: bool, upstream: Option<String>) -> String {
    cookbook_with("", local, upstream)
}

/// [`cookbook`], with further settings of the profile in `profile`.
fn cookbook_with(profile: &str, local: bool, upstream: Option<String>) -> String {
    format!(
        r#"
models:
  - name: "mock-model"
    enabled: true
    profiles:
      - id: "default"
{model_path}        idle_timeout_seconds: 60
        max_instances: 1
        llama_server_args: ""
{profile}{upstream}"#,
        model_path = if local {
            "        model_path: \"./models/mock.gguf\"\n"
        } else {
            ""
        },
        upstream = upstream.unwrap_or_default(),
    )
}

fn config_yaml(spec: &Spec, mock_script: &std::path::Path) -> String {
    let auth = match spec.auth_key {
        Some(key) => format!(
            "auth:\n  enabled: true\n  required_header: \"x-api-key\"\n  allowed_keys: [\"{key}\"]\n"
        ),
        None => String::new(),
    };
    format!(
        r#"
node_id: "test-{suffix}"
listen_addr: "{listen_addr}"
metrics_path: "./tests/metrics_{suffix}.json"
max_vram_mb: 1048576
max_sysmem_mb: 1024
default_model: "{MODEL}"
model_defaults:
  # The mock server has one slot.
  max_concurrent_requests_per_instance: 1
  max_queue_size_per_model: 10
  max_instances_per_model: 1
  max_wait_in_queue_ms: 20000
llama_cpp_ports:
  ranges:
    - start: {port_start}
      end: {port_end}
llama_cpp:
  repo_url: ""
  repo_path: "."
  build_path: "."
  binary_path: "{mock}"
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
{auth}"#,
        suffix = spec.suffix,
        listen_addr = spec.listen_addr,
        port_start = spec.port_start,
        port_end = spec.port_start + 9,
        mock = mock_script.display(),
    )
}

/// Start a node with `cookbook`, run `body`, then stop the node and remove
/// its files whatever the outcome, and re-raise a failure from `body`.
async fn with_node<F, Fut>(spec: Spec, cookbook: String, body: F)
where
    F: FnOnce(Ctx) -> Fut,
    Fut: Future<Output = ()>,
{
    let root = std::env::current_dir().unwrap();
    let mock_script = setup_mock_script(&root, spec.suffix).await;
    let config_path = root.join(format!("tests/config_{}.yaml", spec.suffix));
    let cookbook_path = root.join(format!("tests/cookbook_{}.yaml", spec.suffix));
    let metrics_path = reset_metrics_file(&root, spec.suffix).await;
    tokio::fs::write(&config_path, config_yaml(&spec, &mock_script))
        .await
        .unwrap();
    tokio::fs::write(&cookbook_path, cookbook).await.unwrap();

    cleanup_by_port_range_pattern(spec.port_pattern).await;
    let mut child = tokio::process::Command::new(llamesh_binary(&root))
        .arg("--config")
        .arg(&config_path)
        .arg("--cookbook")
        .arg(&cookbook_path)
        .kill_on_drop(true)
        .spawn()
        .expect("failed to start llamesh");

    let mut headers = reqwest::header::HeaderMap::new();
    if let Some(key) = spec.auth_key {
        headers.insert("x-api-key", key.parse().unwrap());
    }
    let ctx = Ctx {
        base_url: format!("http://{}", spec.listen_addr),
        node_id: format!("test-{}", spec.suffix),
        pid: child.id().expect("llamesh has exited"),
        client: reqwest::Client::builder()
            .default_headers(headers)
            .timeout(Duration::from_secs(30))
            .build()
            .unwrap(),
        cookbook_path: cookbook_path.clone(),
    };
    let outcome = if wait_for_ready(&ctx.base_url).await {
        AssertUnwindSafe(body(ctx)).catch_unwind().await
    } else {
        Err(Box::new(format!("{}: llamesh did not become ready", spec.suffix)) as _)
    };

    graceful_stop(&mut child).await;
    cleanup_procs(&format!("mock_server_{}.sh", spec.suffix)).await;
    cleanup_by_port_range_pattern(spec.port_pattern).await;
    for path in [&config_path, &cookbook_path, &mock_script, &metrics_path] {
        let _ = tokio::fs::remove_file(path).await;
    }
    let _ = tokio::fs::remove_file(metrics_path.with_extension("tmp")).await;

    if let Err(panic) = outcome {
        std::panic::resume_unwind(panic);
    }
}

/// Poll `check` every 50 ms until it holds; panic after `timeout`.
async fn wait_until<F, Fut>(what: &str, timeout: Duration, mut check: F)
where
    F: FnMut() -> Fut,
    Fut: Future<Output = bool>,
{
    let deadline = Instant::now() + timeout;
    while !check().await {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

struct Reply {
    status: StatusCode,
    headers: reqwest::header::HeaderMap,
    body: String,
}

impl Reply {
    fn is_upstream(&self) -> bool {
        self.headers
            .get("x-llamesh-backend")
            .is_some_and(|v| v == "upstream")
    }

    fn assert_upstream(&self) {
        assert_eq!(self.status, StatusCode::OK, "body: {}", self.body);
        assert!(self.is_upstream(), "not from upstream: {:?}", self.headers);
        assert!(self.body.contains("from the stub"), "body: {}", self.body);
    }

    fn assert_local(&self) {
        assert_eq!(self.status, StatusCode::OK, "body: {}", self.body);
        assert!(!self.is_upstream(), "from upstream: {:?}", self.headers);
        assert!(self.body.contains("mock response"), "body: {}", self.body);
    }
}

impl Ctx {
    async fn post(&self, path: &str, body: String) -> Reply {
        let response = self
            .client
            .post(format!("{}{path}", self.base_url))
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .expect("request failed");
        Reply {
            status: response.status(),
            headers: response.headers().clone(),
            body: response.text().await.unwrap_or_default(),
        }
    }

    async fn chat(&self) -> Reply {
        self.post("/v1/chat/completions", chat_body()).await
    }

    async fn get_json(&self, path: &str) -> Value {
        match self
            .client
            .get(format!("{}{path}", self.base_url))
            .send()
            .await
        {
            Ok(response) => response.json().await.unwrap_or_default(),
            Err(_) => Value::Null,
        }
    }

    async fn metrics(&self) -> String {
        let response = self
            .client
            .get(format!("{}/metrics", self.base_url))
            .send()
            .await
            .expect("metrics request failed");
        response.text().await.unwrap_or_default()
    }

    async fn metric(&self, name: &str) -> Option<u64> {
        let prefix = format!("{name}{{model=\"{MODEL}\"}} ");
        self.metrics()
            .await
            .lines()
            .find_map(|line| line.strip_prefix(&prefix)?.trim().parse().ok())
    }

    /// The profile's `metadata.upstream` from `/v1/models`, or null.
    async fn upstream_metadata(&self) -> Value {
        let models = self.get_json("/v1/models").await;
        models["data"]
            .as_array()
            .and_then(|data| {
                data.iter().find(|m| {
                    m["metadata"]["model"] == "mock-model"
                        && m["metadata"]["profile_id"] == "default"
                })
            })
            .map(|m| m["metadata"]["upstream"].clone())
            .unwrap_or(Value::Null)
    }

    async fn wait_healthy(&self, healthy: bool) {
        wait_until(
            &format!("upstream healthy = {healthy}"),
            Duration::from_secs(10),
            || async { self.upstream_metadata().await["healthy"] == healthy },
        )
        .await;
    }

    async fn active_instances(&self) -> u64 {
        let nodes = self.get_json("/cluster/nodes").await;
        nodes["nodes"][&self.node_id]["active_instances"]
            .as_u64()
            .expect("active_instances missing from /cluster/nodes")
    }

    /// Requests the node holds, whether sent to a backend or waiting for one.
    async fn wait_for_requests(&self, count: u64) {
        wait_until(
            &format!("{count} requests in flight"),
            Duration::from_secs(10),
            || async {
                let nodes = self.get_json("/cluster/nodes").await;
                nodes["nodes"][&self.node_id]["current_requests"] == count
            },
        )
        .await;
    }

    /// Send a chat completion in the background.
    fn spawn_chat(&self) -> tokio::task::JoinHandle<Reply> {
        let ctx = self.clone();
        tokio::spawn(async move { ctx.chat().await })
    }

    async fn write_cookbook(&self, cookbook: String) {
        tokio::fs::write(&self.cookbook_path, cookbook)
            .await
            .unwrap();
    }
}

fn chat_body() -> String {
    json!({
        "model": MODEL,
        "messages": [{"role": "user", "content": "hi"}],
        "temperature": 0.25,
    })
    .to_string()
}

fn spec(
    suffix: &'static str,
    listen: &'static str,
    port_start: u16,
    pattern: &'static str,
) -> Spec {
    Spec {
        suffix,
        listen_addr: listen,
        port_start,
        port_pattern: pattern,
        auth_key: None,
    }
}

// =============================================================================
// Tests
// =============================================================================

#[tokio::test]
async fn healthy_upstream_serves_requests() {
    let stub = Stub::start(200, Mode::ok()).await;
    let book = cookbook(true, Some(upstream_yaml(&stub.url, "")));
    let s = spec("ext_up_serve", "127.0.0.1:9270", 13400, "1340[0-9]");
    with_node(s, book, |ctx| async move {
        ctx.wait_healthy(true).await;
        let sent = chat_body();
        let reply = ctx.post("/v1/chat/completions", sent.clone()).await;
        reply.assert_upstream();
        assert_eq!(reply.body, STUB_BODY);

        let posts = stub.posts();
        assert_eq!(posts.len(), 1);
        let received = &posts[0];
        assert_eq!(received.path, "/v1/chat/completions");
        assert_eq!(received.body, sent.as_bytes(), "body was altered");
        assert_eq!(
            received.header("content-length"),
            Some(sent.len().to_string().as_str())
        );
        assert_eq!(received.header("transfer-encoding"), None);

        assert_eq!(ctx.active_instances().await, 0);
        let metadata = ctx.upstream_metadata().await;
        assert_eq!(metadata, json!({"url": stub.url, "healthy": true}));
        assert_eq!(ctx.metric("proxy_upstream_healthy").await, Some(1));
        assert_eq!(ctx.metric("proxy_upstream_requests_total").await, Some(1));
        assert_eq!(ctx.metric("proxy_upstream_failures_total").await, Some(0));
    })
    .await;
}

#[tokio::test]
async fn unhealthy_upstream_falls_back_and_returns() {
    let stub = Stub::start(503, Mode::ok()).await;
    let book = cookbook(true, Some(upstream_yaml(&stub.url, "")));
    let s = spec("ext_up_fallback", "127.0.0.1:9271", 13410, "1341[0-9]");
    with_node(s, book, |ctx| async move {
        // Wait for failed checks so the upstream is known down, not just new.
        wait_until("failed health checks", Duration::from_secs(10), || async {
            stub.recorded("GET").len() >= 2
        })
        .await;
        ctx.wait_healthy(false).await;
        ctx.chat().await.assert_local();
        assert!(stub.posts().is_empty());

        stub.set_health(200);
        ctx.wait_healthy(true).await;
        ctx.chat().await.assert_upstream();
    })
    .await;
}

#[tokio::test]
async fn closed_connection_is_retried_locally() {
    let stub = Stub::start(200, Mode::Close).await;
    let book = cookbook(true, Some(upstream_yaml(&stub.url, "")));
    let s = spec("ext_up_close", "127.0.0.1:9272", 13420, "1342[0-9]");
    with_node(s, book, |ctx| async move {
        ctx.wait_healthy(true).await;
        ctx.chat().await.assert_local();
        assert_eq!(stub.posts().len(), 1, "the upstream was not tried");
        assert_eq!(ctx.metric("proxy_upstream_failures_total").await, Some(1));
        // Tried at the upstream and served locally, the request is still one.
        let metrics = ctx.get_json("/metrics/json").await;
        assert_eq!(metrics["requests_total"], 1);
        assert_eq!(metrics["current_requests"], 0);
    })
    .await;
}

#[tokio::test]
async fn error_status_is_relayed() {
    let error_body = r#"{"error":{"message":"bad prompt","type":"invalid_request_error"}}"#;
    let stub = Stub::start(
        200,
        Mode::Respond {
            status: 400,
            body: error_body.to_string(),
            headers: vec![("X-Stub-Reason".to_string(), "bad-prompt".to_string())],
            delay: Duration::ZERO,
        },
    )
    .await;
    let book = cookbook(true, Some(upstream_yaml(&stub.url, "")));
    let s = spec("ext_up_error", "127.0.0.1:9273", 13430, "1343[0-9]");
    with_node(s, book, |ctx| async move {
        ctx.wait_healthy(true).await;
        let reply = ctx.chat().await;
        assert_eq!(reply.status, StatusCode::BAD_REQUEST);
        assert_eq!(reply.body, error_body);
        assert!(reply.is_upstream(), "headers: {:?}", reply.headers);
        assert_eq!(
            reply.headers.get("x-stub-reason").map(|v| v.as_bytes()),
            Some(&b"bad-prompt"[..])
        );
        assert_eq!(ctx.active_instances().await, 0);
    })
    .await;
}

#[tokio::test]
async fn requests_take_turns_at_the_upstream() {
    let stub = Stub::start(200, Mode::ok_after(Duration::from_millis(300))).await;
    let book = cookbook(true, Some(upstream_yaml(&stub.url, "")));
    let s = spec("ext_up_turns", "127.0.0.1:9274", 13440, "1344[0-9]");
    with_node(s, book, |ctx| async move {
        ctx.wait_healthy(true).await;
        let replies = futures::future::join_all((0..3).map(|_| ctx.chat())).await;
        for reply in &replies {
            reply.assert_upstream();
        }
        assert_eq!(stub.posts().len(), 3);
        assert_eq!(stub.max_active(), 1, "the upstream saw requests at once");
        assert_eq!(ctx.active_instances().await, 0);
    })
    .await;
}

#[tokio::test]
async fn health_loss_moves_pending_requests_locally() {
    let stub = Stub::start(200, Mode::Hold).await;
    let book = cookbook(true, Some(upstream_yaml(&stub.url, "")));
    let s = spec("ext_up_loss", "127.0.0.1:9275", 13450, "1345[0-9]");
    with_node(s, book, |ctx| async move {
        ctx.wait_healthy(true).await;
        let held = ctx.spawn_chat();
        wait_until("the held request", Duration::from_secs(10), || async {
            stub.held() == 1
        })
        .await;
        let waiting = ctx.spawn_chat();
        ctx.wait_for_requests(2).await;

        stub.set_health(500);
        held.await.unwrap().assert_local();
        waiting.await.unwrap().assert_local();
        assert_eq!(
            stub.posts().len(),
            1,
            "the waiting request reached the upstream"
        );
        wait_until(
            "the held connection to close",
            Duration::from_secs(10),
            || async { stub.held_closed() == 1 },
        )
        .await;
    })
    .await;
}

#[tokio::test]
async fn client_disconnect_closes_upstream_connection() {
    let stub = Stub::start(200, Mode::Hold).await;
    let book = cookbook(true, Some(upstream_yaml(&stub.url, "")));
    let s = spec("ext_up_cancel", "127.0.0.1:9276", 13460, "1346[0-9]");
    with_node(s, book, |ctx| async move {
        ctx.wait_healthy(true).await;
        let request = ctx.spawn_chat();
        wait_until("the held request", Duration::from_secs(10), || async {
            stub.held() == 1
        })
        .await;
        request.abort();
        // llamesh polls for client disconnects every 5 s.
        wait_until(
            "the upstream connection to close",
            Duration::from_secs(12),
            || async { stub.held_closed() == 1 },
        )
        .await;
        assert_eq!(ctx.active_instances().await, 0);
    })
    .await;
}

#[tokio::test]
async fn upstream_only_profile() {
    let embeddings =
        r#"{"object":"list","data":[{"object":"embedding","index":0,"embedding":[0.5]}]}"#;
    let stub = Stub::start(
        200,
        Mode::Respond {
            status: 200,
            body: embeddings.to_string(),
            headers: Vec::new(),
            delay: Duration::ZERO,
        },
    )
    .await;
    let book = cookbook(false, Some(upstream_yaml(&stub.url, "")));
    let s = spec("ext_up_only", "127.0.0.1:9277", 13470, "1347[0-9]");
    with_node(s, book, |ctx| async move {
        ctx.wait_healthy(true).await;
        let body = json!({"model": MODEL, "input": "hello"}).to_string();
        let reply = ctx.post("/v1/embeddings", body).await;
        assert_eq!(reply.status, StatusCode::OK, "body: {}", reply.body);
        assert!(reply.is_upstream());
        assert_eq!(reply.body, embeddings);
        assert_eq!(stub.posts()[0].path, "/v1/embeddings");

        let prewarm = ctx
            .post("/admin/prewarm", json!({"model": MODEL}).to_string())
            .await;
        assert_eq!(prewarm.status, StatusCode::BAD_REQUEST, "{}", prewarm.body);

        stub.set_health(500);
        ctx.wait_healthy(false).await;
        let reply = ctx.chat().await;
        assert_eq!(
            reply.status,
            StatusCode::SERVICE_UNAVAILABLE,
            "{}",
            reply.body
        );
        let error: Value = serde_json::from_str(&reply.body).unwrap_or_default();
        assert_eq!(
            error["error"]["type"], "upstream_unavailable",
            "{}",
            reply.body
        );
        assert!(reply.headers.contains_key("retry-after"));
        assert_eq!(ctx.active_instances().await, 0);
    })
    .await;
}

#[tokio::test]
async fn model_name_and_credentials() {
    let stub = Stub::start(200, Mode::ok()).await;
    let extra = "          model: \"other-name\"\n          api_key: \"upstream-secret\"\n";
    let book = cookbook(true, Some(upstream_yaml(&stub.url, extra)));
    let mut s = spec("ext_up_auth", "127.0.0.1:9278", 13480, "1348[0-9]");
    s.auth_key = Some("client-key");
    with_node(s, book, |ctx| async move {
        ctx.wait_healthy(true).await;
        let response = ctx
            .client
            .post(format!("{}/v1/chat/completions", ctx.base_url))
            .header("content-type", "application/json")
            .header("authorization", "Bearer client-token")
            .body(chat_body())
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);

        let posts = stub.posts();
        let received = &posts[0];
        assert_eq!(
            received.header("authorization"),
            Some("Bearer upstream-secret")
        );
        assert_eq!(received.header("x-api-key"), None);
        let body: Value = serde_json::from_slice(&received.body).unwrap();
        let mut expected: Value = serde_json::from_str(&chat_body()).unwrap();
        expected["model"] = json!("other-name");
        assert_eq!(body, expected);
        let checks = stub.recorded("GET");
        assert!(
            checks
                .iter()
                .all(|c| c.header("authorization") == Some("Bearer upstream-secret")),
            "a health check lacked the upstream credentials"
        );
    })
    .await;
}

#[tokio::test]
async fn cookbook_reload_adds_and_removes_upstream() {
    let stub = Stub::start(200, Mode::ok()).await;
    let s = spec("ext_up_reload", "127.0.0.1:9279", 13490, "1349[0-9]");
    with_node(s, cookbook(true, None), |ctx| async move {
        ctx.chat().await.assert_local();

        ctx.write_cookbook(cookbook(true, Some(upstream_yaml(&stub.url, ""))))
            .await;
        ctx.wait_healthy(true).await;
        ctx.chat().await.assert_upstream();

        ctx.write_cookbook(cookbook(true, None)).await;
        wait_until(
            "the upstream to be removed",
            Duration::from_secs(10),
            || async { ctx.upstream_metadata().await.is_null() },
        )
        .await;
        ctx.chat().await.assert_local();
        assert_eq!(stub.posts().len(), 1);
    })
    .await;
}

#[tokio::test]
async fn streamed_response_is_relayed_as_it_arrives() {
    let events: Vec<String> = (0..3)
        .map(|i| json!({"choices": [{"delta": {"content": format!("part{i}")}}]}).to_string())
        .chain(["[DONE]".to_string()])
        .collect();
    let gap = Duration::from_millis(300);
    let stub = Stub::start(200, Mode::Stream { events, gap }).await;
    let book = cookbook(true, Some(upstream_yaml(&stub.url, "")));
    let s = spec("ext_up_stream", "127.0.0.1:9280", 13500, "1350[0-9]");
    with_node(s, book, |ctx| async move {
        ctx.wait_healthy(true).await;
        let body = json!({"model": MODEL, "stream": true, "messages": [{"role": "user", "content": "hi"}]})
            .to_string();
        let response = ctx
            .client
            .post(format!("{}/v1/chat/completions", ctx.base_url))
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            response.headers().get("x-llamesh-backend").map(|v| v.as_bytes()),
            Some(&b"upstream"[..])
        );

        // Started while the first stream is still running.
        let second = ctx.spawn_chat();

        let mut stream = response.bytes_stream();
        let mut text = String::new();
        let mut first_at = None;
        while let Some(chunk) = stream.next().await {
            text.push_str(&String::from_utf8_lossy(&chunk.unwrap()));
            first_at.get_or_insert_with(Instant::now);
        }
        let spread = first_at.unwrap().elapsed();
        assert!(text.contains("part0") && text.contains("[DONE]"), "{text}");
        assert!(
            spread >= gap * 2,
            "the stream arrived at once ({spread:?}), not as sent"
        );

        let second = second.await.unwrap();
        assert_eq!(second.status, StatusCode::OK);
        assert!(second.is_upstream());
        assert_eq!(stub.posts().len(), 2);
        assert_eq!(stub.max_active(), 1, "the second request did not wait");
    })
    .await;
}

#[tokio::test]
async fn waiting_for_a_turn_is_bounded_like_a_queue() {
    let stub = Stub::start(200, Mode::Hold).await;
    let book = cookbook_with(
        "        max_queue_size: 1\n        max_wait_in_queue_ms: 1500\n",
        true,
        Some(upstream_yaml(&stub.url, "")),
    );
    let s = spec("ext_up_bounds", "127.0.0.1:9281", 13510, "1351[0-9]");
    with_node(s, book, |ctx| async move {
        ctx.wait_healthy(true).await;
        let held = ctx.spawn_chat();
        wait_until("the held request", Duration::from_secs(10), || async {
            stub.held() == 1
        })
        .await;
        let waiting = ctx.spawn_chat();
        ctx.wait_for_requests(2).await;

        // The line has room for one request.
        let rejected = ctx.chat().await;
        assert_eq!(rejected.status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(rejected.body.contains("queue_full"), "{}", rejected.body);

        // A busy upstream is not an unhealthy one: the waiting request is
        // not served by a local instance, and gives up when its wait is over.
        let waited = waiting.await.unwrap();
        assert_eq!(waited.status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(waited.body.contains("queue_timeout"), "{}", waited.body);
        assert_eq!(ctx.active_instances().await, 0);
        assert_eq!(stub.posts().len(), 1);

        stub.release();
        held.await.unwrap().assert_upstream();
    })
    .await;
}

#[tokio::test]
async fn fallback_after_bounds_the_wait_and_a_changed_upstream_is_replaced() {
    let stub = Stub::start(200, Mode::Hold).await;
    let book = |url: &str, fallback_after_ms: u64| {
        let extra = format!("          fallback_after_ms: {fallback_after_ms}\n");
        cookbook(true, Some(upstream_yaml(url, &extra)))
    };
    // The same upstream under another spelling, to tell the two settings apart.
    let respelled = format!("{}/", stub.url);
    let s = spec("ext_up_wait", "127.0.0.1:9282", 13520, "1352[0-9]");
    with_node(s, book(&stub.url, 400), |ctx| async move {
        ctx.wait_healthy(true).await;
        let first = ctx.spawn_chat();
        wait_until("the held request", Duration::from_secs(10), || async {
            stub.held() == 1
        })
        .await;

        // The second request waits its 400 ms, then a local instance serves it.
        let asked = Instant::now();
        ctx.chat().await.assert_local();
        assert!(asked.elapsed() >= Duration::from_millis(400));

        // Changing the upstream replaces it. The request the old one is
        // working on carries on, and the new one has its own turns to give.
        ctx.write_cookbook(book(&respelled, 0)).await;
        wait_until("the changed upstream", Duration::from_secs(10), || async {
            ctx.upstream_metadata().await == json!({"url": respelled, "healthy": true})
        })
        .await;
        let second = ctx.spawn_chat();
        wait_until("both held requests", Duration::from_secs(10), || async {
            stub.held() == 2
        })
        .await;

        // With no wait allowed, a request that finds no free turn goes
        // straight to the local instance.
        ctx.chat().await.assert_local();
        assert_eq!(stub.posts().len(), 2);

        stub.release();
        first.await.unwrap().assert_upstream();
        second.await.unwrap().assert_upstream();
    })
    .await;
}

#[tokio::test]
async fn drain_fails_waiting_requests_and_lets_sent_ones_finish() {
    let stub = Stub::start(200, Mode::Hold).await;
    let book = cookbook(true, Some(upstream_yaml(&stub.url, "")));
    let s = spec("ext_up_drain", "127.0.0.1:9283", 13530, "1353[0-9]");
    with_node(s, book, |ctx| async move {
        ctx.wait_healthy(true).await;
        let sent = ctx.spawn_chat();
        wait_until("the held request", Duration::from_secs(10), || async {
            stub.held() == 1
        })
        .await;
        let waiting = ctx.spawn_chat();
        ctx.wait_for_requests(2).await;

        let signalled = tokio::process::Command::new("kill")
            .arg(ctx.pid.to_string())
            .status()
            .await
            .unwrap();
        assert!(signalled.success());

        // The request still waiting its turn is refused, as one queued for
        // a local instance would be.
        let refused = tokio::time::timeout(Duration::from_secs(10), waiting)
            .await
            .expect("the waiting request outlived the drain")
            .unwrap();
        assert_eq!(refused.status, StatusCode::SERVICE_UNAVAILABLE);
        assert!(refused.body.contains("draining"), "{}", refused.body);

        // The request already at the upstream is in flight: the node waits
        // for its response and relays it.
        assert_eq!(stub.held_closed(), 0);
        stub.release();
        sent.await.unwrap().assert_upstream();
    })
    .await;
}

#[tokio::test]
async fn removing_an_upstream_moves_waiting_requests_on() {
    let stub = Stub::start(200, Mode::Hold).await;
    let book = cookbook(true, Some(upstream_yaml(&stub.url, "")));
    let s = spec("ext_up_retire", "127.0.0.1:9284", 13540, "1354[0-9]");
    with_node(s, book, |ctx| async move {
        ctx.wait_healthy(true).await;
        let sent = ctx.spawn_chat();
        wait_until("the held request", Duration::from_secs(10), || async {
            stub.held() == 1
        })
        .await;
        let waiting = ctx.spawn_chat();
        ctx.wait_for_requests(2).await;

        // The cookbook drops the upstream while it is busy. The request
        // waiting for a turn is served without it at once.
        ctx.write_cookbook(cookbook(true, None)).await;
        tokio::time::timeout(Duration::from_secs(10), waiting)
            .await
            .expect("the waiting request stayed with the removed upstream")
            .unwrap()
            .assert_local();

        // The request it was already working on is left to finish.
        assert_eq!(stub.held_closed(), 0);
        stub.release();
        sent.await.unwrap().assert_upstream();
        assert_eq!(stub.posts().len(), 1);
    })
    .await;
}

#[tokio::test]
async fn broken_stream_is_reported_and_frees_the_turn() {
    let stub = Stub::start(200, Mode::BrokenStream).await;
    let book = cookbook(true, Some(upstream_yaml(&stub.url, "")));
    let s = spec("ext_up_broken", "127.0.0.1:9285", 13550, "1355[0-9]");
    with_node(s, book, |ctx| async move {
        ctx.wait_healthy(true).await;
        let body = json!({"model": MODEL, "stream": true, "messages": [{"role": "user", "content": "hi"}]})
            .to_string();
        let response = ctx
            .client
            .post(format!("{}/v1/chat/completions", ctx.base_url))
            .header("content-type", "application/json")
            .body(body)
            .send()
            .await
            .unwrap();
        // The head had been relayed when the stream broke off, so the client
        // sees a response that ends early, not another backend's answer.
        assert_eq!(response.status(), StatusCode::OK);
        let mut stream = response.bytes_stream();
        let mut text = String::new();
        let mut broke_off = false;
        while let Some(chunk) = stream.next().await {
            match chunk {
                Ok(bytes) => text.push_str(&String::from_utf8_lossy(&bytes)),
                Err(_) => broke_off = true,
            }
        }
        assert!(text.contains("part0"), "{text}");
        assert!(broke_off, "the stream ended as if it were complete");

        wait_until("the failure to be counted", Duration::from_secs(10), || async {
            ctx.get_json("/metrics/json").await["errors_total"] == 1
        })
        .await;
        assert_eq!(ctx.active_instances().await, 0);

        // The broken stream gave its turn back.
        stub.set_mode(Mode::ok());
        ctx.chat().await.assert_upstream();
        assert_eq!(ctx.get_json("/metrics/json").await["current_requests"], 0);
    })
    .await;
}
