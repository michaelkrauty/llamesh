//! Noise-encrypted transport layer for cluster communication.
//!
//! Provides helpers for handling HTTP-over-Noise:
//! - recv_request, send_request, send_response_head for request/response transport
//! - recv_data, send_data for raw encrypted transport

use super::handshake::NoiseSession;
use super::{NoiseError, Result};
use crate::noise::NoiseContext;
use axum::http::HeaderMap;
use bytes::Bytes;
use futures::Stream;
use std::future::Future;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Maximum frame size for noise transport
const MAX_FRAME_SIZE: usize = 65535 - 16; // Leave room for AEAD tag

/// Maximum total message size (64 MiB) to prevent OOM from malicious peers
const MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024;

type NoiseBodyReadFuture =
    Pin<Box<dyn Future<Output = Result<(NoiseSession, TcpStream, Vec<u8>)>> + Send>>;

// ============================================================================
// Server-side helpers for handling incoming Noise connections
// ============================================================================

/// Parsed HTTP request from noise transport
#[derive(Debug)]
pub struct NoiseRequest {
    pub method: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

/// Parsed HTTP response head from noise transport.
#[derive(Debug)]
pub struct NoiseResponseHead {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub first_body: Bytes,
}

/// Streaming response body received over Noise.
pub struct NoiseBodyStream {
    session: Option<NoiseSession>,
    stream: Option<TcpStream>,
    first_body: Option<Bytes>,
    pending: Option<NoiseBodyReadFuture>,
    done: bool,
    /// Per-chunk inactivity bound on body reads. `None` disables this bound.
    /// When set, each `recv_data` is wrapped in this timeout (reset per chunk),
    /// so a peer that goes silent mid-body cannot hold this node's slot forever.
    inactivity: Option<Duration>,
    /// The request's `max_request_duration_ms` deadline, armed once for the
    /// whole body: a read still pending when it passes fails. `None` when the
    /// request has no duration limit.
    deadline: Option<Pin<Box<tokio::time::Sleep>>>,
}

/// Client-side response over Noise.
pub struct NoiseHttpResponse {
    pub head: NoiseResponseHead,
    session: NoiseSession,
    stream: TcpStream,
    /// The deadline the request started with, carried into the body stream.
    deadline: Option<tokio::time::Instant>,
}

impl NoiseHttpResponse {
    pub fn into_body_stream(self, inactivity: Option<Duration>) -> NoiseBodyStream {
        NoiseBodyStream {
            session: Some(self.session),
            stream: Some(self.stream),
            first_body: if self.head.first_body.is_empty() {
                None
            } else {
                Some(self.head.first_body)
            },
            pending: None,
            done: false,
            inactivity,
            deadline: self
                .deadline
                .map(|deadline| Box::pin(tokio::time::sleep_until(deadline))),
        }
    }
}

pub struct OutboundNoiseRequest<'a> {
    pub peer_base_url: &'a str,
    pub expected_peer_node_id: Option<&'a str>,
    pub method: &'a str,
    pub path: &'a str,
    pub headers: &'a HeaderMap,
    pub body: &'a [u8],
    pub timeout_duration: Option<Duration>,
}

/// Receive an encrypted HTTP request from a noise connection (server-side)
pub async fn recv_request(
    session: &mut NoiseSession,
    stream: &mut TcpStream,
) -> Result<NoiseRequest> {
    let data = recv_data(session, stream, None).await?;
    parse_http_request(&data)
}

/// Send an encrypted HTTP request over an already-handshaken Noise connection.
pub async fn send_request(
    session: &mut NoiseSession,
    stream: &mut TcpStream,
    method: &str,
    path: &str,
    headers: &HeaderMap,
    body: &[u8],
) -> Result<()> {
    let mut request = format!("{method} {path} HTTP/1.1\r\n");
    for (name, value) in headers {
        if let Ok(value) = value.to_str() {
            request.push_str(name.as_str());
            request.push_str(": ");
            request.push_str(value);
            request.push_str("\r\n");
        }
    }
    request.push_str(&format!("Content-Length: {}\r\n", body.len()));
    request.push_str("\r\n");

    let mut payload = request.into_bytes();
    payload.extend_from_slice(body);
    send_data(session, stream, &payload).await
}

/// Open a Noise connection, send one HTTP request, and read the response head.
pub async fn request(
    context: &NoiseContext,
    request: OutboundNoiseRequest<'_>,
) -> Result<NoiseHttpResponse> {
    request_with_connect_timeout(context, request, crate::cluster::PEER_CONNECT_TIMEOUT).await
}

async fn request_with_connect_timeout(
    context: &NoiseContext,
    request: OutboundNoiseRequest<'_>,
    connect_timeout: Duration,
) -> Result<NoiseHttpResponse> {
    let deadline = request
        .timeout_duration
        .map(|duration| tokio::time::Instant::now() + duration);
    let fut = async {
        let (mut session, mut stream) = tokio::time::timeout(
            connect_timeout,
            connect(
                context,
                request.peer_base_url,
                request.expected_peer_node_id,
            ),
        )
        .await
        .map_err(|_| {
            NoiseError::Transport(format!(
                "Timed out after {}s establishing a connection to {}",
                connect_timeout.as_secs_f64(),
                request.peer_base_url
            ))
        })??;

        send_request(
            &mut session,
            &mut stream,
            request.method,
            request.path,
            request.headers,
            request.body,
        )
        .await?;
        let head = recv_response_head(&mut session, &mut stream).await?;

        Ok(NoiseHttpResponse {
            head,
            session,
            stream,
            deadline,
        })
    };

    match deadline {
        Some(deadline) => tokio::time::timeout_at(deadline, fut)
            .await
            .map_err(|_| NoiseError::Transport("Noise request timed out".into()))?,
        None => fut.await,
    }
}

/// Connect to a peer, complete the Noise handshake, and verify its identity.
async fn connect(
    context: &NoiseContext,
    peer_base_url: &str,
    expected_peer_node_id: Option<&str>,
) -> Result<(NoiseSession, TcpStream)> {
    let connect_addr = connect_addr_from_url(peer_base_url)?;
    let mut stream = TcpStream::connect(connect_addr).await?;
    crate::cluster::set_peer_tcp_keepalive(&stream)?;
    let session = crate::noise::handshake::initiate(
        &mut stream,
        &context.keypair,
        &context.cluster_token,
        &context.node_id,
    )
    .await?;

    let peer_node_id = session.peer_node_id().unwrap_or("unknown");
    if let Some(expected_peer_node_id) = expected_peer_node_id {
        if peer_node_id != expected_peer_node_id {
            return Err(NoiseError::Transport(format!(
                "Peer node_id mismatch: expected {expected_peer_node_id}, got {peer_node_id}"
            )));
        }
    }

    let peer_public_key = format!(
        "noise25519:{}",
        base64::Engine::encode(
            &base64::engine::general_purpose::STANDARD,
            session.peer_public_key()
        )
    );
    crate::noise::known_peers::verify_peer(
        &context.known_peers,
        peer_node_id,
        &peer_public_key,
        &context.config,
    )?;

    Ok((session, stream))
}

/// Send only an encrypted HTTP response head.
pub async fn send_response_head(
    session: &mut NoiseSession,
    stream: &mut TcpStream,
    status: u16,
    status_text: &str,
    headers: &HeaderMap,
) -> Result<()> {
    let mut response = format!("HTTP/1.1 {status} {status_text}\r\n");
    for (name, value) in headers {
        if name.as_str().eq_ignore_ascii_case("content-length")
            || name.as_str().eq_ignore_ascii_case("transfer-encoding")
        {
            continue;
        }
        if let Ok(value) = value.to_str() {
            response.push_str(name.as_str());
            response.push_str(": ");
            response.push_str(value);
            response.push_str("\r\n");
        }
    }
    response.push_str("\r\n");
    send_data(session, stream, response.as_bytes()).await
}

/// Send an encrypted body chunk as one Noise message.
pub async fn send_body_chunk(
    session: &mut NoiseSession,
    stream: &mut TcpStream,
    body: &[u8],
) -> Result<()> {
    send_data(session, stream, body).await
}

/// Send an encrypted end-of-body marker.
pub async fn send_body_end(session: &mut NoiseSession, stream: &mut TcpStream) -> Result<()> {
    send_data(session, stream, &[]).await
}

/// Receive an encrypted HTTP response head.
pub async fn recv_response_head(
    session: &mut NoiseSession,
    stream: &mut TcpStream,
) -> Result<NoiseResponseHead> {
    let data = recv_data(session, stream, None).await?;
    parse_http_response_head(&data)
}

/// Fill `buf` completely, optionally bounding the wait by an inactivity
/// timeout. The timeout is applied to each individual read and reset on every
/// successful read, so it bounds *silence* on the connection (true read-
/// inactivity semantics): a stream that keeps delivering bytes — even slowly,
/// and even within one large frame — never trips it, while one that goes quiet
/// for `duration` does. `None` leaves the read unbounded.
async fn read_exact_bounded(
    stream: &mut TcpStream,
    buf: &mut [u8],
    inactivity: Option<Duration>,
) -> Result<()> {
    let Some(duration) = inactivity else {
        stream.read_exact(buf).await?;
        return Ok(());
    };
    let mut filled = 0;
    while filled < buf.len() {
        let n = tokio::time::timeout(duration, stream.read(&mut buf[filled..]))
            .await
            .map_err(|_| {
                NoiseError::Transport(
                    "Peer response body read timed out (no data within \
                     upstream_read_timeout_ms)"
                        .into(),
                )
            })??;
        if n == 0 {
            // Peer closed the connection before the frame was complete.
            return Err(NoiseError::Transport(
                "Peer closed connection mid-frame".into(),
            ));
        }
        filled += n;
    }
    Ok(())
}

/// Low-level: receive encrypted data with length prefix. `inactivity` bounds
/// the wait for each frame read; `None` leaves the reads unbounded.
pub async fn recv_data(
    session: &mut NoiseSession,
    stream: &mut TcpStream,
    inactivity: Option<Duration>,
) -> Result<Vec<u8>> {
    let mut result = Vec::new();

    loop {
        let mut len_buf = [0u8; 4];
        read_exact_bounded(stream, &mut len_buf, inactivity).await?;
        let len = u32::from_be_bytes(len_buf) as usize;

        if len == 0 {
            break; // End of message
        }

        if len > MAX_FRAME_SIZE + 16 {
            return Err(NoiseError::Transport("Frame too large".into()));
        }

        let mut encrypted = vec![0u8; len];
        read_exact_bounded(stream, &mut encrypted, inactivity).await?;

        let decrypted = session.decrypt(&encrypted)?;
        result.extend_from_slice(&decrypted);

        if result.len() > MAX_MESSAGE_SIZE {
            return Err(NoiseError::Transport("Message too large".into()));
        }
    }

    Ok(result)
}

/// Low-level: send encrypted data with length prefix
pub async fn send_data(
    session: &mut NoiseSession,
    stream: &mut TcpStream,
    data: &[u8],
) -> Result<()> {
    // Split into chunks if needed
    for chunk in data.chunks(MAX_FRAME_SIZE) {
        let encrypted = session.encrypt(chunk)?;
        let len = (encrypted.len() as u32).to_be_bytes();
        stream.write_all(&len).await?;
        stream.write_all(&encrypted).await?;
    }

    // Send zero-length frame to indicate end
    stream.write_all(&0u32.to_be_bytes()).await?;
    stream.flush().await?;

    Ok(())
}

/// Find the byte offset of the `\r\n\r\n` header/body boundary in a raw HTTP
/// frame, searching the raw bytes so the returned offset stays consistent with
/// slicing the original buffer for the body.
fn find_header_boundary(data: &[u8]) -> Option<usize> {
    data.windows(4).position(|w| w == b"\r\n\r\n")
}

/// Parse HTTP request from bytes
fn parse_http_request(data: &[u8]) -> Result<NoiseRequest> {
    // Find the header/body boundary on the raw bytes so `body_start` stays
    // consistent with slicing `data` for the body below. Searching a
    // String::from_utf8_lossy view would expand any non-ASCII header byte to a
    // 3-byte U+FFFD and shift the offset relative to the original buffer.
    let header_end = find_header_boundary(data)
        .ok_or_else(|| NoiseError::Transport("Invalid HTTP request: no header boundary".into()))?;
    let body_start = header_end + 4;
    let header_part = String::from_utf8_lossy(&data[..header_end]);

    // Parse request line
    let mut lines = header_part.lines();
    let request_line = lines
        .next()
        .ok_or_else(|| NoiseError::Transport("No request line".into()))?;

    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        return Err(NoiseError::Transport("Invalid request line".into()));
    }

    let method = parts[0].to_string();
    let path = parts[1].to_string();

    // Parse headers
    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }

    // Extract body
    let body = Bytes::copy_from_slice(data.get(body_start..).unwrap_or(&[]));

    Ok(NoiseRequest {
        method,
        path,
        headers,
        body,
    })
}

fn parse_http_response_head(data: &[u8]) -> Result<NoiseResponseHead> {
    // Boundary on the raw bytes so `body_start` matches the original buffer
    // (see parse_http_request).
    let header_end = find_header_boundary(data)
        .ok_or_else(|| NoiseError::Transport("Invalid HTTP response: no header boundary".into()))?;
    let body_start = header_end + 4;
    let header_part = String::from_utf8_lossy(&data[..header_end]);
    let mut lines = header_part.lines();
    let status_line = lines
        .next()
        .ok_or_else(|| NoiseError::Transport("No status line".into()))?;

    let mut parts = status_line.splitn(3, ' ');
    let _version = parts
        .next()
        .ok_or_else(|| NoiseError::Transport("Invalid status line".into()))?;
    let status = parts
        .next()
        .ok_or_else(|| NoiseError::Transport("Invalid status line".into()))?
        .parse::<u16>()
        .map_err(|_| NoiseError::Transport("Invalid HTTP status".into()))?;
    let _status_text = parts.next().unwrap_or("");

    let mut headers = Vec::new();
    for line in lines {
        if let Some((name, value)) = line.split_once(':') {
            headers.push((name.trim().to_string(), value.trim().to_string()));
        }
    }

    Ok(NoiseResponseHead {
        status,
        headers,
        first_body: Bytes::copy_from_slice(data.get(body_start..).unwrap_or(&[])),
    })
}

fn connect_addr_from_url(peer_base_url: &str) -> Result<(String, u16)> {
    let url_text = if peer_base_url.contains("://") {
        peer_base_url.to_string()
    } else {
        format!("http://{peer_base_url}")
    };
    let url = reqwest::Url::parse(&url_text)
        .map_err(|e| NoiseError::Transport(format!("Invalid peer URL: {e}")))?;
    let host = url
        .host_str()
        .ok_or_else(|| NoiseError::Transport("Peer URL missing host".into()))?
        // URL hosts retain IPv6 brackets; the TCP (host, port) tuple must not.
        .trim_matches(['[', ']'])
        .to_string();
    let port = url
        .port_or_known_default()
        .ok_or_else(|| NoiseError::Transport("Peer URL missing port".into()))?;
    Ok((host, port))
}

impl Stream for NoiseBodyStream {
    type Item = Result<Bytes>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        if self.done {
            return Poll::Ready(None);
        }

        if self
            .deadline
            .as_mut()
            .is_some_and(|deadline| deadline.as_mut().poll(cx).is_ready())
        {
            self.done = true;
            self.pending = None;
            self.session = None;
            self.stream = None;
            self.first_body = None;
            return Poll::Ready(Some(Err(NoiseError::Transport(
                "Noise request timed out".into(),
            ))));
        }

        if let Some(bytes) = self.first_body.take() {
            return Poll::Ready(Some(Ok(bytes)));
        }

        if self.pending.is_none() {
            let mut session = self.session.take().expect("noise session missing");
            let mut stream = self.stream.take().expect("noise stream missing");
            // `recv_data` applies `inactivity` to each frame read, so a
            // slow-but-active peer keeps resetting the bound while one that
            // stops sending trips it — even for a multi-frame body chunk.
            let inactivity = self.inactivity;
            self.pending = Some(Box::pin(async move {
                let bytes = recv_data(&mut session, &mut stream, inactivity).await?;
                Ok((session, stream, bytes))
            }));
        }

        let pending = self.pending.as_mut().expect("pending future missing");
        match pending.as_mut().poll(cx) {
            Poll::Pending => Poll::Pending,
            Poll::Ready(Ok((session, stream, bytes))) => {
                self.pending = None;
                self.session = Some(session);
                self.stream = Some(stream);
                if bytes.is_empty() {
                    self.done = true;
                    Poll::Ready(None)
                } else {
                    Poll::Ready(Some(Ok(Bytes::from(bytes))))
                }
            }
            Poll::Ready(Err(e)) => {
                self.pending = None;
                self.done = true;
                Poll::Ready(Some(Err(e)))
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn pending_body(deadline: Option<tokio::time::Instant>) -> NoiseBodyStream {
        NoiseBodyStream {
            session: None,
            stream: None,
            first_body: Some(Bytes::from_static(b"chunk")),
            pending: Some(Box::pin(std::future::pending())),
            done: false,
            inactivity: None,
            deadline: deadline.map(|deadline| Box::pin(tokio::time::sleep_until(deadline))),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn test_body_stream_deadline_interrupts_pending_read() {
        use futures::StreamExt;

        let mut body = pending_body(Some(tokio::time::Instant::now() + Duration::from_secs(1)));
        assert_eq!(body.next().await.unwrap().unwrap(), b"chunk"[..]);
        assert!(futures::poll!(body.next()).is_pending());
        tokio::time::advance(Duration::from_secs(1)).await;
        let result = futures::poll!(body.next());
        assert!(matches!(result, Poll::Ready(Some(Err(_)))), "{result:?}");
        assert!(body.next().await.is_none());
        assert!(body.pending.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn test_body_stream_deadline_interrupts_buffering() {
        use futures::TryStreamExt;

        let body = pending_body(Some(tokio::time::Instant::now() + Duration::from_secs(1)));
        let result =
            tokio::time::timeout(Duration::from_secs(2), body.try_collect::<Vec<_>>()).await;
        assert!(matches!(result, Ok(Err(_))), "{result:?}");
    }

    #[tokio::test(start_paused = true)]
    async fn test_body_stream_deadline_precedes_first_byte() {
        use futures::StreamExt;

        let mut body = pending_body(Some(tokio::time::Instant::now() + Duration::from_secs(1)));
        tokio::time::advance(Duration::from_secs(1)).await;
        let result = body.next().await;
        assert!(matches!(result, Some(Err(_))), "{result:?}");
        assert!(body.next().await.is_none());
    }

    #[tokio::test(start_paused = true)]
    async fn test_body_stream_without_deadline_stays_pending() {
        use futures::StreamExt;

        let mut body = pending_body(None);
        tokio::time::advance(Duration::from_secs(86400)).await;
        assert_eq!(body.next().await.unwrap().unwrap(), b"chunk"[..]);
        assert!(futures::poll!(body.next()).is_pending());
    }

    #[test]
    fn test_connect_addr_from_url_preserves_host_and_port() {
        for (url, host, port) in [
            ("http://192.0.2.1:8080", "192.0.2.1", 8080),
            ("mesh.example:9000", "mesh.example", 9000),
            ("https://mesh.example", "mesh.example", 443),
        ] {
            assert_eq!(connect_addr_from_url(url).unwrap(), (host.into(), port));
        }
    }

    #[tokio::test]
    async fn test_connect_addr_from_url_ipv6_is_resolvable_as_tcp_tuple() {
        for url in ["http://[2001:db8::1]:8080", "[2001:db8::1]:8080"] {
            let addr = connect_addr_from_url(url).unwrap();
            assert_eq!(addr, ("2001:db8::1".into(), 8080));
            // Exercise the same tuple resolver as TcpStream::connect without
            // requiring an IPv6 interface or connecting to the example address.
            let resolved: Vec<_> = tokio::net::lookup_host(addr).await.unwrap().collect();
            assert_eq!(resolved, vec!["[2001:db8::1]:8080".parse().unwrap()]);
        }
    }

    #[test]
    fn test_parse_http_request_get() {
        let request = b"GET /api/test HTTP/1.1\r\nHost: localhost\r\n\r\n";
        let parsed = parse_http_request(request).unwrap();
        assert_eq!(parsed.method, "GET");
        assert_eq!(parsed.path, "/api/test");
        assert_eq!(parsed.headers.len(), 1);
        assert_eq!(
            parsed.headers[0],
            ("Host".to_string(), "localhost".to_string())
        );
        assert!(parsed.body.is_empty());
    }

    #[test]
    fn test_parse_http_request_post_with_body() {
        let request = b"POST /api/data HTTP/1.1\r\nContent-Type: application/json\r\nContent-Length: 13\r\n\r\n{\"key\":\"val\"}";
        let parsed = parse_http_request(request).unwrap();
        assert_eq!(parsed.method, "POST");
        assert_eq!(parsed.path, "/api/data");
        assert_eq!(parsed.headers.len(), 2);
        assert_eq!(parsed.body.as_ref(), b"{\"key\":\"val\"}");
    }

    #[test]
    fn test_parse_http_request_no_headers() {
        let request = b"GET / HTTP/1.1\r\n\r\n";
        let parsed = parse_http_request(request).unwrap();
        assert_eq!(parsed.method, "GET");
        assert_eq!(parsed.path, "/");
        assert!(parsed.headers.is_empty());
    }

    #[test]
    fn test_parse_http_request_invalid_no_boundary() {
        let request = b"GET / HTTP/1.1\r\nHost: localhost";
        let result = parse_http_request(request);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_http_request_invalid_empty() {
        let request = b"\r\n\r\n";
        let result = parse_http_request(request);
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_http_request_non_ascii_header_byte_keeps_body_intact() {
        // A non-ASCII byte in the header region must not shift the body slice.
        // The header/body boundary is found on the raw bytes, so the body offset
        // stays consistent with the original buffer. Computing it against a
        // String::from_utf8_lossy view instead would expand each invalid byte to
        // a 3-byte U+FFFD and slice the body in the wrong place.
        let mut request: Vec<u8> = Vec::new();
        request.extend_from_slice(b"POST /v1/x HTTP/1.1\r\nX-Bin: ");
        request.extend_from_slice(&[0xFF, 0xFE]); // invalid UTF-8 in a header value
        request.extend_from_slice(b"\r\n\r\nBODYDATA123");

        let parsed = parse_http_request(&request).unwrap();
        assert_eq!(parsed.method, "POST");
        assert_eq!(parsed.path, "/v1/x");
        assert_eq!(parsed.body.as_ref(), b"BODYDATA123");
    }

    #[test]
    fn test_parse_http_response_head_non_ascii_header_byte_keeps_body_intact() {
        let mut response: Vec<u8> = Vec::new();
        response.extend_from_slice(b"HTTP/1.1 200 OK\r\nX-Bin: ");
        response.extend_from_slice(&[0xFF, 0xFE]); // invalid UTF-8 in a header value
        response.extend_from_slice(b"\r\n\r\nFIRSTBODY");

        let parsed = parse_http_response_head(&response).unwrap();
        assert_eq!(parsed.status, 200);
        assert_eq!(parsed.first_body.as_ref(), b"FIRSTBODY");
    }

    fn test_context(dir: &std::path::Path, node_id: &str, token: &str) -> NoiseContext {
        NoiseContext {
            keypair: crate::noise::keypair::NodeKeypair::generate(),
            cluster_token: token.into(),
            known_peers: crate::noise::known_peers::KnownPeers::load_or_create(
                &dir.join(format!("{node_id}-known-peers")),
            )
            .unwrap(),
            config: crate::config::ClusterNoiseConfig::default(),
            node_id: node_id.into(),
        }
    }

    async fn test_body_response(
        timeout: Option<Duration>,
        chunks: Option<usize>,
    ) -> (NoiseHttpResponse, tokio::task::JoinHandle<()>) {
        let dir = tempfile::tempdir().unwrap();
        let token = crate::noise::token::generate();
        let server = test_context(dir.path(), "server-node", &token);
        let client = test_context(dir.path(), "client-node", &token);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let responder = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut session = crate::noise::handshake::respond(
                &mut stream,
                &server.keypair,
                &server.cluster_token,
                &server.node_id,
            )
            .await
            .unwrap();
            recv_request(&mut session, &mut stream).await.unwrap();
            send_response_head(&mut session, &mut stream, 200, "OK", &HeaderMap::new())
                .await
                .unwrap();
            let mut sent = 0;
            while chunks.is_none_or(|chunks| sent < chunks) {
                tokio::time::sleep(Duration::from_millis(10)).await;
                if send_body_chunk(&mut session, &mut stream, b"chunk")
                    .await
                    .is_err()
                {
                    return;
                }
                sent += 1;
            }
            let _ = send_body_end(&mut session, &mut stream).await;
        });
        let response = request(
            &client,
            OutboundNoiseRequest {
                peer_base_url: &url,
                expected_peer_node_id: Some("server-node"),
                method: "POST",
                path: "/v1/chat/completions",
                headers: &HeaderMap::new(),
                body: b"{}",
                timeout_duration: timeout,
            },
        )
        .await
        .unwrap();
        (response, responder)
    }

    async fn assert_request_body_deadline(streaming: bool) {
        use futures::{StreamExt, TryStreamExt};

        let duration = Duration::from_secs(1);
        let started = tokio::time::Instant::now();
        let (response, responder) = test_body_response(Some(duration), None).await;
        let inactivity = streaming.then(|| Duration::from_secs(1));
        let mut body = response.into_body_stream(inactivity);
        let mut chunks = 0;
        let result = tokio::time::timeout(Duration::from_secs(5), async {
            if streaming {
                loop {
                    match body.next().await {
                        Some(Ok(_)) => chunks += 1,
                        Some(Err(error)) => break Err(error),
                        None => break Ok(()),
                    }
                }
            } else {
                body.by_ref()
                    .try_fold(Vec::new(), |mut bytes, chunk| async move {
                        bytes.extend_from_slice(&chunk);
                        Ok(bytes)
                    })
                    .await
                    .map(|_| ())
            }
        })
        .await;
        responder.abort();
        let _ = responder.await;
        let error = result
            .expect("an active peer body must not outlive the request deadline")
            .expect_err("an endless peer body must fail");
        assert!(error.to_string().contains("timed out"), "{error}");
        assert!(started.elapsed() >= duration);
        // `started` precedes key setup and the handshake, which the deadline
        // does not; leave room for them on a slow machine.
        assert!(started.elapsed() < duration + Duration::from_secs(2));
        if streaming {
            assert!(chunks > 1, "active chunks must precede the timeout");
            assert!(body.next().await.is_none());
        }
    }

    #[tokio::test]
    async fn test_request_duration_bounds_streaming_body() {
        assert_request_body_deadline(true).await;
    }

    #[tokio::test]
    async fn test_request_duration_bounds_buffered_body() {
        assert_request_body_deadline(false).await;
    }

    #[tokio::test]
    async fn test_request_body_completes_before_deadline() {
        use futures::TryStreamExt;

        let (response, responder) = test_body_response(Some(Duration::from_secs(5)), Some(3)).await;
        let chunks: Vec<_> = response.into_body_stream(None).try_collect().await.unwrap();
        assert_eq!(chunks.concat(), b"chunkchunkchunk");
        responder.await.unwrap();
    }

    #[tokio::test]
    async fn test_request_body_without_deadline() {
        use futures::TryStreamExt;

        let (response, responder) = test_body_response(None, Some(3)).await;
        tokio::time::sleep(Duration::from_millis(100)).await;
        let chunks: Vec<_> = response.into_body_stream(None).try_collect().await.unwrap();
        assert_eq!(chunks.concat(), b"chunkchunkchunk");
        responder.await.unwrap();
    }

    #[tokio::test]
    async fn test_connect_enables_peer_tcp_keepalive() {
        let dir = tempfile::tempdir().unwrap();
        let token = crate::noise::token::generate();
        let server = test_context(dir.path(), "server-node", &token);
        let client = test_context(dir.path(), "client-node", &token);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        let responder = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            crate::noise::handshake::respond(
                &mut stream,
                &server.keypair,
                &server.cluster_token,
                &server.node_id,
            )
            .await
            .unwrap();
            stream
        });

        let (session, stream) = connect(&client, &url, Some("server-node")).await.unwrap();
        assert_eq!(session.peer_node_id(), Some("server-node"));
        let socket = socket2::SockRef::from(&stream);
        assert!(socket.keepalive().unwrap());
        assert_eq!(
            socket.tcp_keepalive_time().unwrap(),
            crate::cluster::PEER_TCP_KEEPALIVE_TIME
        );
        assert_eq!(
            socket.tcp_keepalive_interval().unwrap(),
            crate::cluster::PEER_TCP_KEEPALIVE_INTERVAL
        );
        assert_eq!(
            socket.tcp_keepalive_retries().unwrap(),
            crate::cluster::PEER_TCP_KEEPALIVE_RETRIES
        );
        drop(responder.await.unwrap());
    }

    #[tokio::test]
    async fn test_request_times_out_when_peer_never_completes_handshake() {
        let dir = tempfile::tempdir().unwrap();
        let client = test_context(dir.path(), "client-node", &crate::noise::token::generate());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}", listener.local_addr().unwrap());
        // Accept the TCP connection, then never answer the handshake.
        let silent_peer = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            let mut sink = Vec::new();
            let _ = stream.read_to_end(&mut sink).await;
        });

        let headers = HeaderMap::new();
        let started = std::time::Instant::now();
        let result = request_with_connect_timeout(
            &client,
            OutboundNoiseRequest {
                peer_base_url: &url,
                expected_peer_node_id: None,
                method: "POST",
                path: "/v1/chat/completions",
                headers: &headers,
                body: b"{}",
                timeout_duration: None,
            },
            Duration::from_millis(200),
        )
        .await;

        let Err(NoiseError::Transport(message)) = result else {
            panic!("expected a connection-establishment timeout");
        };
        assert!(message.contains("establishing a connection"), "{message}");
        assert!(started.elapsed() < Duration::from_secs(5));
        silent_peer.abort();
    }

    #[test]
    fn test_noise_request_debug() {
        let request = NoiseRequest {
            method: "GET".to_string(),
            path: "/test".to_string(),
            headers: vec![],
            body: Bytes::new(),
        };
        let debug_str = format!("{request:?}");
        assert!(debug_str.contains("GET"));
        assert!(debug_str.contains("/test"));
    }
}
