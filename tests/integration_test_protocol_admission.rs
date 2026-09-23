#![cfg(target_os = "linux")]

use std::{net::SocketAddr, process::Child, time::Duration};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpStream,
    time::{sleep, timeout},
};

const FAST: Duration = Duration::from_secs(2);
const DETECT_MS: u64 = 10_000;

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
  request_body_limit_bytes: 1048576
  idle_timeout_seconds: 60
  protocol_detect_timeout_ms: {detect_ms}
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
        timeout(Duration::from_secs(10), async {
            loop {
                assert!(
                    server.child.try_wait().unwrap().is_none(),
                    "server exited during startup"
                );
                if let Ok(mut stream) = TcpStream::connect(addr).await {
                    version_on_socket(
                        &mut stream,
                        b"GET /version HTTP/1.1\r\nHost: localhost\r\nConnection: close\r\n\r\n",
                    )
                    .await;
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("server startup timed out");
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
