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

/// Serve requests the way `llama-server` does. A request to `/stream` gets a
/// streamed (chunked) response that advertises keep-alive, after which the
/// server is finished with the connection, as `llama-server` is after every
/// streamed response. Any other request gets a sized keep-alive response. The
/// real server closes the connection right after the terminal chunk; here a
/// request that arrives on a finished connection is closed without a response,
/// which reproduces deterministically the race a client loses when it reuses
/// the connection before noticing the close. A request carrying
/// `Connection: close` is answered in kind and its connection then closed, as
/// cpp-httplib does.
async fn serve_like_llama_server(listener: TcpListener, connections: Arc<AtomicUsize>) {
    loop {
        let Ok((stream, _)) = listener.accept().await else {
            return;
        };
        connections.fetch_add(1, Ordering::SeqCst);
        tokio::spawn(serve_llama_server_connection(stream));
    }
}

async fn serve_llama_server_connection(mut stream: TcpStream) {
    let mut finished = false;
    let mut buf = Vec::new();
    while let Some(head) = read_request(&mut stream, &mut buf).await {
        if finished {
            return; // Close without responding.
        }
        let close = head
            .to_ascii_lowercase()
            .contains("\r\nconnection: close\r\n");
        let connection = if close {
            "connection: close"
        } else {
            "keep-alive: timeout=5, max=100"
        };
        let response = if head.starts_with("POST /stream ") {
            finished = true;
            format!(
                "HTTP/1.1 200 OK\r\n{connection}\r\ntransfer-encoding: chunked\r\n\r\n\
                 5\r\ndata:\r\n0\r\n\r\n"
            )
        } else {
            format!("HTTP/1.1 200 OK\r\n{connection}\r\ncontent-length: 2\r\n\r\nok")
        };
        if stream.write_all(response.as_bytes()).await.is_err() || close {
            return;
        }
    }
}

/// Read one request, consuming its `content-length` body, and return its head.
async fn read_request(stream: &mut TcpStream, buf: &mut Vec<u8>) -> Option<String> {
    let mut chunk = [0u8; 1024];
    loop {
        if let Some(end) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            let head_len = end + 4;
            let head = String::from_utf8_lossy(&buf[..head_len]).into_owned();
            let body_len = head
                .lines()
                .find_map(|line| {
                    let line = line.to_ascii_lowercase();
                    let value = line.strip_prefix("content-length:")?;
                    value.trim().parse::<usize>().ok()
                })
                .unwrap_or(0);
            while buf.len() < head_len + body_len {
                match stream.read(&mut chunk).await {
                    Ok(0) | Err(_) => return None,
                    Ok(n) => buf.extend_from_slice(&chunk[..n]),
                }
            }
            buf.drain(..head_len + body_len);
            return Some(head);
        }
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return None,
            Ok(n) => buf.extend_from_slice(&chunk[..n]),
        }
    }
}

async fn start_llama_like_server() -> (String, Arc<AtomicUsize>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}", listener.local_addr().unwrap());
    let connections = Arc::new(AtomicUsize::new(0));
    tokio::spawn(serve_like_llama_server(listener, connections.clone()));
    (base, connections)
}

/// Send a request the way the router forwards one to a local instance.
async fn forward(
    client: &reqwest::Client,
    base: &str,
    path: &str,
    response_streaming: bool,
) -> reqwest::Result<String> {
    let headers = crate::router::build_local_upstream_headers(
        &reqwest::header::HeaderMap::new(),
        response_streaming,
    );
    client
        .post(format!("{base}{path}"))
        .headers(headers)
        .body("{}")
        .send()
        .await?
        .text()
        .await
}

#[tokio::test]
async fn streamed_requests_do_not_reuse_connections_the_server_closes() {
    let (base, connections) = start_llama_like_server().await;
    let client = build_local_upstream_client(0).unwrap();
    for _ in 0..3 {
        assert_eq!(
            forward(&client, &base, "/stream", true).await.unwrap(),
            "data:"
        );
    }
    assert_eq!(connections.load(Ordering::SeqCst), 3);
}

#[tokio::test]
async fn reusing_a_connection_after_a_streamed_response_fails() {
    // Without `Connection: close`, the connection is pooled after the stream
    // and the next request is sent on it: the failure the router prevents.
    let (base, _) = start_llama_like_server().await;
    let client = build_local_upstream_client(0).unwrap();
    assert_eq!(
        forward(&client, &base, "/stream", false).await.unwrap(),
        "data:"
    );
    let error = forward(&client, &base, "/stream", false).await.unwrap_err();
    assert!(error.is_request(), "unexpected error: {error:?}");
}

#[tokio::test]
async fn sized_responses_keep_reusing_connections() {
    let (base, connections) = start_llama_like_server().await;
    let client = build_local_upstream_client(0).unwrap();
    for _ in 0..3 {
        assert_eq!(
            forward(&client, &base, "/sized", false).await.unwrap(),
            "ok"
        );
    }
    assert_eq!(connections.load(Ordering::SeqCst), 1);
}
