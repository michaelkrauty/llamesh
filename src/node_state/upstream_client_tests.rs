use super::*;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::{TcpListener, TcpStream};

/// Serve keep-alive HTTP/1.1 like `llama-server`, except that a connection the
/// server has already expired is closed when the next request arrives, instead
/// of at the expiry instant. This reproduces, deterministically, the race where
/// a client reuses a pooled connection the server is closing: the request is
/// written to a connection that will never answer it.
async fn serve_with_unobserved_keepalive_expiry(
    listener: TcpListener,
    expiry: Duration,
    connections: Arc<AtomicUsize>,
) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        connections.fetch_add(1, Ordering::SeqCst);
        tokio::spawn(serve_connection(stream, expiry));
    }
}

async fn serve_connection(mut stream: TcpStream, expiry: Duration) {
    let mut idle_since: Option<Instant> = None;
    let mut buf = Vec::new();
    loop {
        // Read one request head; the test only sends body-less GETs.
        while !buf.windows(4).any(|w| w == b"\r\n\r\n") {
            let mut chunk = [0u8; 1024];
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => return,
                Ok(n) => buf.extend_from_slice(&chunk[..n]),
            }
        }
        buf.clear();
        if idle_since.is_some_and(|since| since.elapsed() >= expiry) {
            return; // Expired: close without responding.
        }
        let response = b"HTTP/1.1 200 OK\r\ncontent-length: 2\r\n\r\nok";
        if stream.write_all(response).await.is_err() {
            return;
        }
        idle_since = Some(Instant::now());
    }
}

async fn start_server(expiry: Duration) -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let url = format!("http://{}/health", listener.local_addr().unwrap());
    let connections = Arc::new(AtomicUsize::new(0));
    tokio::spawn(serve_with_unobserved_keepalive_expiry(
        listener,
        expiry,
        connections.clone(),
    ));
    (url, connections)
}

async fn get_ok(client: &reqwest::Client, url: &str) {
    let response = client.get(url).send().await.unwrap();
    assert_eq!(response.status(), reqwest::StatusCode::OK);
    assert_eq!(response.text().await.unwrap(), "ok");
}

#[tokio::test]
async fn local_upstream_client_reuses_connections_between_back_to_back_requests() {
    let (url, connections) = start_server(Duration::from_secs(60)).await;
    let client = build_local_upstream_client(0).unwrap();
    for _ in 0..3 {
        get_ok(&client, &url).await;
    }
    assert_eq!(connections.load(Ordering::SeqCst), 1);
}

#[tokio::test]
async fn local_upstream_client_does_not_reuse_connections_past_idle_timeout() {
    // Any server keep-alive longer than the pool timeout, such as llama-server's
    // 5 seconds, behaves like this one for a request sent after the gap.
    let expiry = LOCAL_UPSTREAM_POOL_IDLE_TIMEOUT + Duration::from_millis(250);
    let (url, connections) = start_server(expiry).await;
    let client = build_local_upstream_client(0).unwrap();
    get_ok(&client, &url).await;
    tokio::time::sleep(expiry + Duration::from_millis(250)).await;
    get_ok(&client, &url).await;
    assert_eq!(connections.load(Ordering::SeqCst), 2);
}
