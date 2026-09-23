//! Real TLS ingress coverage. Requires openssl; alternate loopback source IPs require Linux.
#![cfg(target_os = "linux")]

use reqwest::{Certificate, Client, Identity, StatusCode};
use serde_json::{json, Value};
use std::{
    fs,
    net::{IpAddr, TcpListener},
    path::Path,
    process::{Child, Command, Stdio},
    time::{Duration, Instant},
};
use tempfile::TempDir;

struct Fixture {
    child: Child,
    dir: TempDir,
    url: String,
    observer: Client,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn openssl(dir: &Path, args: &[&str]) {
    let output = Command::new("openssl")
        .args(args)
        .current_dir(dir)
        .output()
        .expect("openssl is required for TLS gossip integration tests");
    assert!(
        output.status.success(),
        "openssl {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
}

fn certificates(dir: &Path) {
    for ca in ["ca", "rogue-ca"] {
        openssl(
            dir,
            &[
                "req",
                "-x509",
                "-newkey",
                "ec",
                "-pkeyopt",
                "ec_paramgen_curve:P-256",
                "-nodes",
                "-days",
                "1",
                "-subj",
                &format!("/CN={ca}"),
                "-keyout",
                &format!("{ca}.key"),
                "-out",
                &format!("{ca}.pem"),
                "-addext",
                "basicConstraints=critical,CA:TRUE",
                "-addext",
                "keyUsage=critical,keyCertSign,cRLSign",
            ],
        );
    }
    fs::write(
        dir.join("leaf.ext"),
        "basicConstraints=critical,CA:FALSE\nkeyUsage=critical,digitalSignature\nextendedKeyUsage=serverAuth,clientAuth\nsubjectAltName=IP:127.0.0.1\n",
    )
    .unwrap();
    for (name, cn, ca) in [
        ("server", "receiver", "ca"),
        ("peer", "peer", "ca"),
        ("wrong", "wrong-identity", "ca"),
        ("no-cn", "", "ca"),
        ("untrusted", "peer", "rogue-ca"),
    ] {
        openssl(
            dir,
            &[
                "req",
                "-new",
                "-newkey",
                "ec",
                "-pkeyopt",
                "ec_paramgen_curve:P-256",
                "-nodes",
                "-subj",
                &if cn.is_empty() {
                    "/O=test".into()
                } else {
                    format!("/CN={cn}")
                },
                "-keyout",
                &format!("{name}.key"),
                "-out",
                &format!("{name}.csr"),
            ],
        );
        openssl(
            dir,
            &[
                "x509",
                "-req",
                "-in",
                &format!("{name}.csr"),
                "-CA",
                &format!("{ca}.pem"),
                "-CAkey",
                &format!("{ca}.key"),
                "-CAcreateserial",
                "-days",
                "1",
                "-extfile",
                "leaf.ext",
                "-out",
                &format!("{name}.pem"),
            ],
        );
    }
}

fn client(dir: &Path, identity: Option<&str>, source: &str) -> Client {
    let mut builder = Client::builder()
        .no_proxy()
        .http1_only()
        .timeout(Duration::from_secs(3))
        .local_address(source.parse::<IpAddr>().unwrap())
        .tls_built_in_root_certs(false)
        .add_root_certificate(
            Certificate::from_pem(&fs::read(dir.join("ca.pem")).unwrap()).unwrap(),
        );
    if let Some(name) = identity {
        let mut pem = fs::read(dir.join(format!("{name}.pem"))).unwrap();
        pem.extend(fs::read(dir.join(format!("{name}.key"))).unwrap());
        builder = builder.identity(Identity::from_pem(&pem).unwrap());
    }
    builder.build().unwrap()
}

impl Fixture {
    async fn start(cluster_tls: bool) -> Self {
        let dir = tempfile::tempdir().unwrap();
        certificates(dir.path());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let port = listener.local_addr().unwrap().port();
        fs::write(dir.path().join("cookbook.yaml"), "models: []\n").unwrap();
        fs::write(
            dir.path().join("config.yaml"),
            format!(
                r#"node_id: receiver
listen_addr: "127.0.0.1:{port}"
max_vram_mb: 0
max_sysmem_mb: 0
default_model: unused
metrics_path: ./metrics.json
model_defaults:
  max_concurrent_requests_per_instance: 1
  max_queue_size_per_model: 1
  max_instances_per_model: 1
llama_cpp:
  enabled: false
  repo_url: unused
  branch: unused
  auto_update_interval_seconds: 0
cluster:
  enabled: false
  peers: []
  gossip_interval_seconds: 1
  discovery: {{mdns: false}}
  noise: {{enabled: false}}
http:
  request_body_limit_bytes: 1048576
  idle_timeout_seconds: 5
server_tls:
  enabled: true
  cert_path: ./server.pem
  key_path: ./server.key
cluster_tls:
  enabled: {cluster_tls}
  ca_cert_path: ./ca.pem
  client_cert_path: ./server.pem
  client_key_path: ./server.key
"#
            ),
        )
        .unwrap();
        let log = fs::File::create(dir.path().join("child.log")).unwrap();
        let binary = std::env::var_os("LLAMESH_TLS_GOSSIP_BINARY")
            .unwrap_or_else(|| env!("CARGO_BIN_EXE_llamesh").into());
        let observer = client(dir.path(), None, "127.0.0.1");
        drop(listener);
        let child = Command::new(binary)
            .args(["--config", "config.yaml", "--cookbook", "cookbook.yaml"])
            .current_dir(dir.path())
            .env_clear()
            .env("HOME", dir.path())
            .env("TMPDIR", dir.path())
            .env("NO_PROXY", "*")
            .stdin(Stdio::null())
            .stdout(log.try_clone().unwrap())
            .stderr(log)
            .spawn()
            .unwrap();
        let mut fixture = Self {
            child,
            dir,
            url: format!("https://127.0.0.1:{port}"),
            observer,
        };
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            if let Ok(response) = fixture
                .observer
                .get(format!("{}/version", fixture.url))
                .send()
                .await
            {
                if response.status().is_success() {
                    let version = response.json::<Value>().await.unwrap();
                    assert!(version["version"].as_str().is_some_and(|v| !v.is_empty()));
                    return fixture;
                }
            }
            assert!(
                fixture.child.try_wait().unwrap().is_none() && Instant::now() < deadline,
                "TLS child failed to start: {}",
                fs::read_to_string(fixture.dir.path().join("child.log")).unwrap()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    async fn nodes(&self) -> Value {
        self.observer
            .get(format!("{}/cluster/nodes", self.url))
            .send()
            .await
            .unwrap()
            .error_for_status()
            .unwrap()
            .json::<Value>()
            .await
            .unwrap()["nodes"]
            .clone()
    }

    async fn origin(&self) -> Value {
        let mut origin = self.nodes().await["receiver"].clone();
        origin["node_id"] = json!("peer");
        origin["address"] = json!("https://127.0.0.1:31001");
        origin
    }
}

#[tokio::test]
async fn tls_gossip_infers_and_refreshes_source_address() {
    let fixture = Fixture::start(true).await;
    let mut origin = fixture.origin().await;
    for (source, port) in [("127.0.0.2", 31001), ("127.0.0.3", 31002)] {
        origin["address"] = json!(format!("https://127.0.0.1:{port}"));
        let response = client(fixture.dir.path(), Some("peer"), source)
            .post(format!("{}/cluster/gossip", fixture.url))
            .header("x-forwarded-for", "127.0.0.9")
            .header("forwarded", "for=127.0.0.9;proto=http;host=127.0.0.9:31999")
            .json(&json!({"origin": origin, "known_peers": []}))
            .send()
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        assert_eq!(
            fixture.nodes().await["peer"]["address"],
            format!("https://{source}:{port}"),
            "authenticated TLS gossip must use the socket IP and advertised listener scheme/port"
        );
    }
}

#[tokio::test]
async fn tls_gossip_rejects_unauthenticated_and_mismatched_origins() {
    let fixture = Fixture::start(true).await;
    let mut origin = fixture.origin().await;
    let response = client(fixture.dir.path(), Some("peer"), "127.0.0.2")
        .post(format!("{}/cluster/gossip", fixture.url))
        .json(&json!({"origin": origin, "known_peers": []}))
        .send()
        .await
        .unwrap();
    assert_eq!(
        response.status(),
        StatusCode::OK,
        "matching client identity must be accepted"
    );
    let before = fixture.nodes().await["peer"].clone();
    assert_eq!(
        before["node_id"], "peer",
        "accepted gossip must insert peer"
    );
    origin["address"] = json!("https://127.0.0.1:31999");
    origin["current_requests"] = json!(123);
    for (identity, plaintext, expected) in [
        (None, false, Some(StatusCode::UNAUTHORIZED)),
        (Some("wrong"), false, Some(StatusCode::FORBIDDEN)),
        (Some("no-cn"), false, Some(StatusCode::UNAUTHORIZED)),
        (Some("untrusted"), false, None),
        (None, true, Some(StatusCode::UNAUTHORIZED)),
    ] {
        for node_id in ["peer", "impostor"] {
            origin["node_id"] = json!(node_id);
            let url = if plaintext {
                fixture.url.replace("https://", "http://")
            } else {
                fixture.url.clone()
            };
            let result = client(fixture.dir.path(), identity, "127.0.0.3")
                .post(format!("{url}/cluster/gossip"))
                .json(&json!({"origin": origin, "known_peers": []}))
                .send()
                .await;
            match expected {
                Some(status) => assert_eq!(
                    result.unwrap().status(),
                    status,
                    "{identity:?}, plaintext={plaintext}"
                ),
                None => assert!(
                    result.is_err(),
                    "untrusted client certificate must fail TLS"
                ),
            }
            let nodes = fixture.nodes().await;
            assert_eq!(
                nodes["peer"], before,
                "rejected gossip mutated existing peer"
            );
            assert!(
                nodes.get("impostor").is_none(),
                "rejected gossip inserted impostor"
            );
        }
    }
}

#[tokio::test]
async fn tls_public_https_without_cluster_tls() {
    // Startup verifies a public HTTPS response using the ephemeral CA.
    let fixture = Fixture::start(false).await;
    assert_eq!(fixture.nodes().await["receiver"]["node_id"], "receiver");
}
