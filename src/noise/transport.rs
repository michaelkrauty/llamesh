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
}

/// Client-side response over Noise.
pub struct NoiseHttpResponse {
    pub head: NoiseResponseHead,
    session: NoiseSession,
    stream: TcpStream,
}

impl NoiseHttpResponse {
    pub fn into_body_stream(self) -> NoiseBodyStream {
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
    let data = recv_data(session, stream).await?;
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
    let fut = async {
        let connect_addr = connect_addr_from_url(request.peer_base_url)?;
        let mut stream = TcpStream::connect(connect_addr).await?;
        let mut session = crate::noise::handshake::initiate(
            &mut stream,
            &context.keypair,
            &context.cluster_token,
            &context.node_id,
        )
        .await?;

        let peer_node_id = session.peer_node_id().unwrap_or("unknown");
        if let Some(expected_peer_node_id) = request.expected_peer_node_id {
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
        })
    };

    match request.timeout_duration {
        Some(duration) => tokio::time::timeout(duration, fut)
            .await
            .map_err(|_| NoiseError::Transport("Noise request timed out".into()))?,
        None => fut.await,
    }
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
    let data = recv_data(session, stream).await?;
    parse_http_response_head(&data)
}

/// Low-level: receive encrypted data with length prefix
pub async fn recv_data(session: &mut NoiseSession, stream: &mut TcpStream) -> Result<Vec<u8>> {
    let mut result = Vec::new();

    loop {
        let mut len_buf = [0u8; 4];
        stream.read_exact(&mut len_buf).await?;
        let len = u32::from_be_bytes(len_buf) as usize;

        if len == 0 {
            break; // End of message
        }

        if len > MAX_FRAME_SIZE + 16 {
            return Err(NoiseError::Transport("Frame too large".into()));
        }

        let mut encrypted = vec![0u8; len];
        stream.read_exact(&mut encrypted).await?;

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

/// Parse HTTP request from bytes
fn parse_http_request(data: &[u8]) -> Result<NoiseRequest> {
    let data_str = String::from_utf8_lossy(data);

    // Find header/body boundary
    let header_end = data_str
        .find("\r\n\r\n")
        .ok_or_else(|| NoiseError::Transport("Invalid HTTP request: no header boundary".into()))?;

    let header_part = &data_str[..header_end];
    let body_start = header_end + 4;

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
    let data_str = String::from_utf8_lossy(data);
    let header_end = data_str
        .find("\r\n\r\n")
        .ok_or_else(|| NoiseError::Transport("Invalid HTTP response: no header boundary".into()))?;

    let header_part = &data_str[..header_end];
    let body_start = header_end + 4;
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

        if let Some(bytes) = self.first_body.take() {
            return Poll::Ready(Some(Ok(bytes)));
        }

        if self.pending.is_none() {
            let mut session = self.session.take().expect("noise session missing");
            let mut stream = self.stream.take().expect("noise stream missing");
            self.pending = Some(Box::pin(async move {
                let bytes = recv_data(&mut session, &mut stream).await?;
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
