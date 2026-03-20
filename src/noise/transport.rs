//! Noise-encrypted transport layer for cluster communication.
//!
//! Provides server-side functions for handling incoming Noise connections:
//! - recv_request, send_response for HTTP-over-Noise
//! - recv_data, send_data for raw encrypted transport

use super::handshake::NoiseSession;
use super::{NoiseError, Result};
use bytes::Bytes;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Maximum frame size for noise transport
const MAX_FRAME_SIZE: usize = 65535 - 16; // Leave room for AEAD tag

/// Maximum total message size (64 MiB) to prevent OOM from malicious peers
const MAX_MESSAGE_SIZE: usize = 64 * 1024 * 1024;

// ============================================================================
// Server-side helpers for handling incoming Noise connections
// ============================================================================

/// Parsed HTTP request from noise transport
#[derive(Debug)]
pub struct NoiseRequest {
    pub method: String,
    pub path: String,
    #[allow(dead_code)] // Parsed but not yet used
    pub headers: Vec<(String, String)>,
    pub body: Bytes,
}

/// Receive an encrypted HTTP request from a noise connection (server-side)
pub async fn recv_request(
    session: &mut NoiseSession,
    stream: &mut TcpStream,
) -> Result<NoiseRequest> {
    let data = recv_data(session, stream).await?;
    parse_http_request(&data)
}

/// Send an encrypted HTTP response over a noise connection (server-side)
pub async fn send_response(
    session: &mut NoiseSession,
    stream: &mut TcpStream,
    status: u16,
    status_text: &str,
    headers: &[(&str, &str)],
    body: &[u8],
) -> Result<()> {
    // Build HTTP response
    let mut response = format!("HTTP/1.1 {} {}\r\n", status, status_text);
    for (name, value) in headers {
        response.push_str(&format!("{}: {}\r\n", name, value));
    }
    response.push_str(&format!("Content-Length: {}\r\n", body.len()));
    response.push_str("\r\n");

    let mut payload = response.into_bytes();
    payload.extend_from_slice(body);

    send_data(session, stream, &payload).await
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
        let debug_str = format!("{:?}", request);
        assert!(debug_str.contains("GET"));
        assert!(debug_str.contains("/test"));
    }
}
