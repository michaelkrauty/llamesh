//! Protocol detection for single-port multiplexing.
//!
//! Detects protocol from first bytes of TCP connection:
//! - TLS ClientHello (0x16 0x03)
//! - HTTP methods (GET, POST, PUT, DELETE, HEAD, OPTIONS, CONNECT, TRACE, PATCH)
//! - HTTP/2 preface ("PRI ")
//! - Noise handshake (default: raw bytes)

use std::io;
use tokio::net::TcpStream;

/// Detected protocol type
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DetectedProtocol {
    /// TLS ClientHello - starts with 0x16 0x03
    Tls,
    /// HTTP/1.x request - starts with method name
    Http,
    /// HTTP/2 connection preface - starts with "PRI "
    Http2,
    /// Noise Protocol handshake - raw bytes (default)
    Noise,
}

/// Peek first bytes of stream to detect protocol without consuming them.
///
/// Uses `TcpStream::peek()` which doesn't advance the read position.
pub async fn detect(stream: &TcpStream) -> io::Result<DetectedProtocol> {
    let mut buf = [0u8; 4];

    // Peek up to 4 bytes - enough to detect all protocols
    let n = stream.peek(&mut buf).await?;

    if n == 0 {
        // Empty peek - treat as noise (connection will fail anyway)
        return Ok(DetectedProtocol::Noise);
    }

    // Check for TLS ClientHello: 0x16 (handshake) followed by version (0x03 0x0X)
    if n >= 2 && buf[0] == 0x16 && buf[1] == 0x03 {
        return Ok(DetectedProtocol::Tls);
    }

    // Check for HTTP/2 connection preface: "PRI " (0x50 0x52 0x49 0x20)
    if n >= 4 && &buf[..4] == b"PRI " {
        return Ok(DetectedProtocol::Http2);
    }

    // Check for HTTP/1.x methods by first byte
    // GET, POST, PUT, DELETE, HEAD, OPTIONS, CONNECT, TRACE, PATCH
    if n >= 1 {
        match buf[0] {
            b'G' | // GET
            b'P' | // POST, PUT, PATCH
            b'D' | // DELETE
            b'H' | // HEAD
            b'O' | // OPTIONS
            b'C' | // CONNECT
            b'T'   // TRACE
                => return Ok(DetectedProtocol::Http),
            _ => {}
        }
    }

    // Default: assume Noise Protocol handshake
    // Noise XX initiator sends 32-byte ephemeral public key (raw bytes)
    Ok(DetectedProtocol::Noise)
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;
    use tokio::net::TcpListener;

    async fn detect_bytes(data: &[u8]) -> DetectedProtocol {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        // Copy data to owned Vec to move into spawned task
        let data_owned = data.to_vec();
        let client = tokio::spawn(async move {
            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            stream.write_all(&data_owned).await.unwrap();
            // Keep connection open for peek
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        });

        let (stream, _) = listener.accept().await.unwrap();
        // Small delay to ensure data is available
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
        let result = detect(&stream).await.unwrap();

        client.abort();
        result
    }

    #[tokio::test]
    async fn test_detect_tls() {
        // TLS ClientHello starts with 0x16 0x03 0x01 (TLS 1.0) or 0x03 0x03 (TLS 1.2)
        let result = detect_bytes(&[0x16, 0x03, 0x01, 0x00]).await;
        assert_eq!(result, DetectedProtocol::Tls);
    }

    #[tokio::test]
    async fn test_detect_http_get() {
        let result = detect_bytes(b"GET / HTTP/1.1\r\n").await;
        assert_eq!(result, DetectedProtocol::Http);
    }

    #[tokio::test]
    async fn test_detect_http_post() {
        let result = detect_bytes(b"POST /api HTTP/1.1\r\n").await;
        assert_eq!(result, DetectedProtocol::Http);
    }

    #[tokio::test]
    async fn test_detect_http_put() {
        let result = detect_bytes(b"PUT /resource HTTP/1.1\r\n").await;
        assert_eq!(result, DetectedProtocol::Http);
    }

    #[tokio::test]
    async fn test_detect_http_delete() {
        let result = detect_bytes(b"DELETE /item HTTP/1.1\r\n").await;
        assert_eq!(result, DetectedProtocol::Http);
    }

    #[tokio::test]
    async fn test_detect_http2() {
        // HTTP/2 connection preface
        let result = detect_bytes(b"PRI * HTTP/2.0\r\n\r\nSM\r\n\r\n").await;
        assert_eq!(result, DetectedProtocol::Http2);
    }

    #[tokio::test]
    async fn test_detect_noise() {
        // Random bytes that don't match any known protocol
        let result = detect_bytes(&[0x00, 0x01, 0x02, 0x03]).await;
        assert_eq!(result, DetectedProtocol::Noise);

        // Typical Noise XX first message (32-byte ephemeral key)
        let noise_bytes: [u8; 32] = [
            0x9a, 0x1b, 0x2c, 0x3d, 0x4e, 0x5f, 0x60, 0x71, 0x82, 0x93, 0xa4, 0xb5, 0xc6, 0xd7,
            0xe8, 0xf9, 0x0a, 0x1b, 0x2c, 0x3d, 0x4e, 0x5f, 0x60, 0x71, 0x82, 0x93, 0xa4, 0xb5,
            0xc6, 0xd7, 0xe8, 0xf9,
        ];
        let result = detect_bytes(&noise_bytes).await;
        assert_eq!(result, DetectedProtocol::Noise);
    }

    #[tokio::test]
    async fn test_detect_http_options() {
        let result = detect_bytes(b"OPTIONS * HTTP/1.1\r\n").await;
        assert_eq!(result, DetectedProtocol::Http);
    }

    #[tokio::test]
    async fn test_detect_http_head() {
        let result = detect_bytes(b"HEAD / HTTP/1.1\r\n").await;
        assert_eq!(result, DetectedProtocol::Http);
    }

    #[tokio::test]
    async fn test_detect_http_trace() {
        let result = detect_bytes(b"TRACE / HTTP/1.1\r\n").await;
        assert_eq!(result, DetectedProtocol::Http);
    }

    #[tokio::test]
    async fn test_detect_http_connect() {
        let result = detect_bytes(b"CONNECT host:443 HTTP/1.1\r\n").await;
        assert_eq!(result, DetectedProtocol::Http);
    }

    #[tokio::test]
    async fn test_detect_http_patch() {
        let result = detect_bytes(b"PATCH /resource HTTP/1.1\r\n").await;
        assert_eq!(result, DetectedProtocol::Http);
    }
}
