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
///
/// A single peek sees only the bytes that have *arrived*, and the first TCP
/// segment can legitimately carry fewer bytes than a signature needs — a
/// fragmented TLS ClientHello delivering just `0x16` must not be classified
/// as Noise, and a lone `P` cannot distinguish HTTP/1.x (`POST`) from the
/// HTTP/2 preface (`PRI `). When the peeked prefix is still ambiguous, this
/// waits for more bytes. The caller bounds the total wait with
/// `protocol_detect_timeout_ms`, which also covers clients that send an
/// ambiguous prefix and then stall.
pub async fn detect(stream: &TcpStream) -> io::Result<DetectedProtocol> {
    let mut buf = [0u8; 4];

    loop {
        let n = stream.peek(&mut buf).await?;

        if n == 0 {
            // EOF before any data - treat as noise (connection will fail anyway)
            return Ok(DetectedProtocol::Noise);
        }

        if let Some(protocol) = classify(&buf[..n]) {
            return Ok(protocol);
        }

        // Ambiguous prefix: more bytes are needed. peek() does not consume,
        // so awaiting readiness again would return immediately with the same
        // bytes; poll with a short sleep instead.
        tokio::time::sleep(std::time::Duration::from_millis(2)).await;
    }
}

/// Classify a peeked prefix, or return `None` when the bytes seen so far are
/// a proper prefix of more than one protocol signature and a decision needs
/// more data.
fn classify(buf: &[u8]) -> Option<DetectedProtocol> {
    match buf[0] {
        // TLS ClientHello: 0x16 (handshake) followed by version (0x03 0x0X)
        0x16 => match buf.get(1) {
            Some(0x03) => Some(DetectedProtocol::Tls),
            Some(_) => Some(DetectedProtocol::Noise),
            None => None,
        },
        // 'P' starts both HTTP/1.x methods (POST, PUT, PATCH) and the HTTP/2
        // connection preface "PRI " (0x50 0x52 0x49 0x20). No HTTP/1.x method
        // starts with "PR", so the second byte picks the branch; the full
        // four bytes confirm the preface.
        b'P' => match buf.get(1) {
            Some(b'R') => {
                if buf.len() >= 4 {
                    if &buf[..4] == b"PRI " {
                        Some(DetectedProtocol::Http2)
                    } else {
                        Some(DetectedProtocol::Http)
                    }
                } else {
                    None
                }
            }
            Some(_) => Some(DetectedProtocol::Http),
            None => None,
        },
        // Remaining HTTP/1.x method initials are unambiguous on their own:
        // GET, DELETE, HEAD, OPTIONS, CONNECT, TRACE
        b'G' | b'D' | b'H' | b'O' | b'C' | b'T' => Some(DetectedProtocol::Http),
        // Anything else: assume Noise Protocol handshake
        // Noise XX initiator sends 32-byte ephemeral public key (raw bytes)
        _ => Some(DetectedProtocol::Noise),
    }
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

    /// Like `detect_bytes`, but delivers the payload in two fragments with a
    /// delay in between, so detection initially peeks an incomplete prefix.
    async fn detect_fragmented(first: &[u8], rest: &[u8]) -> DetectedProtocol {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let first_owned = first.to_vec();
        let rest_owned = rest.to_vec();
        let client = tokio::spawn(async move {
            let mut stream = tokio::net::TcpStream::connect(addr).await.unwrap();
            stream.set_nodelay(true).unwrap();
            stream.write_all(&first_owned).await.unwrap();
            stream.flush().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            stream.write_all(&rest_owned).await.unwrap();
            stream.flush().await.unwrap();
            tokio::time::sleep(std::time::Duration::from_millis(200)).await;
        });

        let (stream, _) = listener.accept().await.unwrap();
        let result = detect(&stream).await.unwrap();

        client.abort();
        result
    }

    #[tokio::test]
    async fn test_detect_tls_fragmented_first_byte() {
        // A TLS ClientHello whose first segment carries only the handshake
        // byte must wait for the version byte, not fall through to Noise.
        let result = detect_fragmented(&[0x16], &[0x03, 0x01, 0x00]).await;
        assert_eq!(result, DetectedProtocol::Tls);
    }

    #[tokio::test]
    async fn test_detect_http2_fragmented_p() {
        // A lone 'P' is ambiguous between HTTP/1.x and the HTTP/2 preface.
        let result = detect_fragmented(b"P", b"RI * HTTP/2.0\r\n\r\nSM\r\n\r\n").await;
        assert_eq!(result, DetectedProtocol::Http2);
    }

    #[tokio::test]
    async fn test_detect_http_post_fragmented_p() {
        let result = detect_fragmented(b"P", b"OST /api HTTP/1.1\r\n").await;
        assert_eq!(result, DetectedProtocol::Http);
    }

    #[tokio::test]
    async fn test_detect_noise_tls_lookalike() {
        // 0x16 followed by a non-TLS version byte is not a ClientHello.
        let result = detect_bytes(&[0x16, 0x42, 0x00, 0x00]).await;
        assert_eq!(result, DetectedProtocol::Noise);
    }

    #[test]
    fn classify_waits_on_ambiguous_prefixes() {
        assert_eq!(classify(&[0x16]), None);
        assert_eq!(classify(b"P"), None);
        assert_eq!(classify(b"PR"), None);
        assert_eq!(classify(b"PRI"), None);
    }

    #[test]
    fn classify_decides_unambiguous_prefixes_immediately() {
        assert_eq!(classify(&[0x16, 0x03]), Some(DetectedProtocol::Tls));
        assert_eq!(classify(&[0x16, 0x42]), Some(DetectedProtocol::Noise));
        assert_eq!(classify(b"PO"), Some(DetectedProtocol::Http));
        assert_eq!(classify(b"PRI "), Some(DetectedProtocol::Http2));
        assert_eq!(classify(b"PRIx"), Some(DetectedProtocol::Http));
        assert_eq!(classify(b"G"), Some(DetectedProtocol::Http));
        assert_eq!(classify(&[0x00]), Some(DetectedProtocol::Noise));
    }
}
