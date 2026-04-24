//! Noise Protocol XX handshake implementation (server-side only).
//!
//! Pattern: Noise_XX_25519_ChaChaPoly_SHA256
//! - XX: Mutual authentication, both sides prove identity
//! - 25519: X25519 DH for key exchange
//! - ChaChaPoly: ChaCha20-Poly1305 AEAD
//! - SHA256: Hash function
//!
//! This module provides server-side (responder) handshake for accepting
//! incoming Noise connections. Client-side (initiator) code is only
//! compiled for tests, as production cluster gossip uses HTTP/mTLS.
//!
//! Token verification:
//! After handshake completes, both sides compute HMAC(token, handshake_hash)
//! and exchange it to verify cluster membership.

use super::keypair::NodeKeypair;
use super::{NoiseError, Result};
use base64::Engine;
use hmac::{Hmac, Mac};
use sha2::Sha256;
use snow::{Builder, TransportState};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

/// Noise protocol pattern
const NOISE_PATTERN: &str = "Noise_XX_25519_ChaChaPoly_SHA256";

/// Maximum message size
const MAX_MESSAGE_SIZE: usize = 65535;

/// Handshake message buffer size
const HANDSHAKE_BUFFER_SIZE: usize = 1024;

type HmacSha256 = Hmac<Sha256>;

/// A completed noise session ready for encrypted transport
pub struct NoiseSession {
    transport: TransportState,
    peer_public_key: [u8; 32],
    peer_node_id: Option<String>,
}

impl NoiseSession {
    /// Get the peer's static public key
    pub fn peer_public_key(&self) -> &[u8; 32] {
        &self.peer_public_key
    }

    /// Get the peer's node ID
    pub fn peer_node_id(&self) -> Option<&str> {
        self.peer_node_id.as_deref()
    }

    /// Encrypt a message
    pub fn encrypt(&mut self, plaintext: &[u8]) -> Result<Vec<u8>> {
        let mut buffer = vec![0u8; plaintext.len() + 16]; // AEAD tag
        let len = self
            .transport
            .write_message(plaintext, &mut buffer)
            .map_err(|e| NoiseError::Transport(format!("Encrypt failed: {}", e)))?;
        buffer.truncate(len);
        Ok(buffer)
    }

    /// Decrypt a message
    pub fn decrypt(&mut self, ciphertext: &[u8]) -> Result<Vec<u8>> {
        let mut buffer = vec![0u8; ciphertext.len()];
        let len = self
            .transport
            .read_message(ciphertext, &mut buffer)
            .map_err(|e| NoiseError::Transport(format!("Decrypt failed: {}", e)))?;
        buffer.truncate(len);
        Ok(buffer)
    }
}

/// Build a responder handshake state
fn build_responder(keypair: &NodeKeypair) -> Result<snow::HandshakeState> {
    let key_bytes = keypair.private_key_bytes();
    Builder::new(
        NOISE_PATTERN
            .parse()
            .map_err(|e| NoiseError::Handshake(format!("Invalid noise pattern: {}", e)))?,
    )
    .local_private_key(&key_bytes)
    .build_responder()
    .map_err(|e| NoiseError::Handshake(format!("Failed to build responder: {}", e)))
}

/// Compute token verification HMAC
fn compute_token_verification(token: &[u8], handshake_hash: &[u8]) -> Vec<u8> {
    let mut mac = HmacSha256::new_from_slice(token).expect("HMAC can take key of any size");
    mac.update(handshake_hash);
    mac.finalize().into_bytes().to_vec()
}

/// Verify token HMAC
fn verify_token(expected: &[u8], received: &[u8]) -> bool {
    if expected.len() != received.len() {
        return false;
    }
    // Constant-time comparison
    let mut result = 0u8;
    for (a, b) in expected.iter().zip(received.iter()) {
        result |= a ^ b;
    }
    result == 0
}

/// Perform responder (server) handshake
pub async fn respond(
    stream: &mut TcpStream,
    keypair: &NodeKeypair,
    cluster_token: &str,
    our_node_id: &str,
) -> Result<NoiseSession> {
    let mut handshake = build_responder(keypair)?;

    let mut buffer = vec![0u8; HANDSHAKE_BUFFER_SIZE];
    let mut read_buf = vec![0u8; HANDSHAKE_BUFFER_SIZE];

    // <- e
    let msg = recv_message(stream, &mut read_buf).await?;
    handshake
        .read_message(&msg, &mut buffer)
        .map_err(|e| NoiseError::Handshake(format!("Read e failed: {}", e)))?;

    // -> e, ee, s, es
    let len = handshake
        .write_message(&[], &mut buffer)
        .map_err(|e| NoiseError::Handshake(format!("Write e,ee,s,es failed: {}", e)))?;
    send_message(stream, &buffer[..len]).await?;

    // <- s, se
    let msg = recv_message(stream, &mut read_buf).await?;
    handshake
        .read_message(&msg, &mut buffer)
        .map_err(|e| NoiseError::Handshake(format!("Read s,se failed: {}", e)))?;

    // Handshake complete - get peer's static key
    let peer_public_key: [u8; 32] = handshake
        .get_remote_static()
        .ok_or_else(|| NoiseError::Handshake("No remote static key".into()))?
        .try_into()
        .map_err(|_| NoiseError::Handshake("Invalid remote static key length".into()))?;

    // Get handshake hash for token verification
    let handshake_hash = handshake.get_handshake_hash().to_vec();

    // Convert to transport mode
    let mut transport = handshake
        .into_transport_mode()
        .map_err(|e| NoiseError::Handshake(format!("Transport mode failed: {}", e)))?;

    // Receive peer's token verification first
    let msg = recv_message(stream, &mut read_buf).await?;
    let mut dec_buf = vec![0u8; msg.len()];
    let len = transport
        .read_message(&msg, &mut dec_buf)
        .map_err(|e| NoiseError::Handshake(format!("Token recv failed: {}", e)))?;

    if len < 32 {
        return Err(NoiseError::Handshake("Token message too short".into()));
    }

    let peer_hmac = &dec_buf[..32];
    let token_bytes = super::token::decode(cluster_token)?;
    let expected_hmac = compute_token_verification(&token_bytes, &handshake_hash);

    if !verify_token(&expected_hmac, peer_hmac) {
        return Err(NoiseError::TokenVerificationFailed);
    }

    // Extract peer node_id
    let peer_node_id = String::from_utf8_lossy(&dec_buf[32..len]).to_string();

    // Send our token verification
    let our_hmac = compute_token_verification(&token_bytes, &handshake_hash);
    let mut payload = our_hmac;
    payload.extend_from_slice(our_node_id.as_bytes());

    let mut enc_buf = vec![0u8; payload.len() + 16];
    let len = transport
        .write_message(&payload, &mut enc_buf)
        .map_err(|e| NoiseError::Handshake(format!("Token send failed: {}", e)))?;
    send_message(stream, &enc_buf[..len]).await?;

    tracing::debug!(
        peer_node_id = %peer_node_id,
        peer_key = %format!("ed25519:{}", base64::engine::general_purpose::STANDARD.encode(peer_public_key)),
        "Handshake complete (responder)"
    );

    Ok(NoiseSession {
        transport,
        peer_public_key,
        peer_node_id: Some(peer_node_id),
    })
}

/// Send a length-prefixed message
async fn send_message(stream: &mut TcpStream, data: &[u8]) -> Result<()> {
    if data.len() > MAX_MESSAGE_SIZE {
        return Err(NoiseError::Handshake("Message too large".into()));
    }

    let len = (data.len() as u16).to_be_bytes();
    stream.write_all(&len).await?;
    stream.write_all(data).await?;
    stream.flush().await?;
    Ok(())
}

/// Receive a length-prefixed message
async fn recv_message(stream: &mut TcpStream, buffer: &mut [u8]) -> Result<Vec<u8>> {
    let mut len_buf = [0u8; 2];
    stream.read_exact(&mut len_buf).await?;
    let len = u16::from_be_bytes(len_buf) as usize;

    if len > MAX_MESSAGE_SIZE {
        return Err(NoiseError::Handshake("Message too large".into()));
    }

    if len > buffer.len() {
        return Err(NoiseError::Handshake("Buffer too small".into()));
    }

    stream.read_exact(&mut buffer[..len]).await?;
    Ok(buffer[..len].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::noise::keypair::NodeKeypair;
    use crate::noise::token;
    use snow::Builder;
    use tokio::net::TcpListener;

    // =========================================================================
    // Client-side (initiator) code - only needed for tests
    // Production cluster gossip uses HTTP/mTLS for outbound communication.
    // =========================================================================

    /// Test-only methods for NoiseSession
    #[cfg(test)]
    #[allow(dead_code)]
    impl NoiseSession {
        /// Get the peer's public key in display format (test-only)
        pub fn peer_public_key_display(&self) -> String {
            format!(
                "ed25519:{}",
                base64::engine::general_purpose::STANDARD.encode(self.peer_public_key)
            )
        }

        /// Set the peer's node ID after verification (test-only)
        pub fn set_peer_node_id(&mut self, node_id: String) {
            self.peer_node_id = Some(node_id);
        }
    }

    /// Build an initiator handshake state (test-only)
    fn build_initiator(keypair: &NodeKeypair) -> Result<snow::HandshakeState> {
        let key_bytes = keypair.private_key_bytes();
        Builder::new(
            NOISE_PATTERN
                .parse()
                .map_err(|e| NoiseError::Handshake(format!("Invalid noise pattern: {}", e)))?,
        )
        .local_private_key(&key_bytes)
        .build_initiator()
        .map_err(|e| NoiseError::Handshake(format!("Failed to build initiator: {}", e)))
    }

    /// Perform initiator (client) handshake (test-only)
    ///
    /// This is the client-side handshake used to test the responder.
    /// Production code only uses the responder (server) side.
    async fn initiate(
        stream: &mut tokio::net::TcpStream,
        keypair: &NodeKeypair,
        cluster_token: &str,
        our_node_id: &str,
    ) -> Result<NoiseSession> {
        let mut handshake = build_initiator(keypair)?;

        let mut buffer = vec![0u8; HANDSHAKE_BUFFER_SIZE];
        let mut read_buf = vec![0u8; HANDSHAKE_BUFFER_SIZE];

        // -> e
        let len = handshake
            .write_message(&[], &mut buffer)
            .map_err(|e| NoiseError::Handshake(format!("Write e failed: {}", e)))?;
        send_message(stream, &buffer[..len]).await?;

        // <- e, ee, s, es
        let msg = recv_message(stream, &mut read_buf).await?;
        handshake
            .read_message(&msg, &mut buffer)
            .map_err(|e| NoiseError::Handshake(format!("Read e,ee,s,es failed: {}", e)))?;

        // -> s, se
        let len = handshake
            .write_message(&[], &mut buffer)
            .map_err(|e| NoiseError::Handshake(format!("Write s,se failed: {}", e)))?;
        send_message(stream, &buffer[..len]).await?;

        // Handshake complete - get peer's static key
        let peer_public_key: [u8; 32] = handshake
            .get_remote_static()
            .ok_or_else(|| NoiseError::Handshake("No remote static key".into()))?
            .try_into()
            .map_err(|_| NoiseError::Handshake("Invalid remote static key length".into()))?;

        // Get handshake hash for token verification
        let handshake_hash = handshake.get_handshake_hash().to_vec();

        // Convert to transport mode
        let mut transport = handshake
            .into_transport_mode()
            .map_err(|e| NoiseError::Handshake(format!("Transport mode failed: {}", e)))?;

        // Token verification: send HMAC(token, handshake_hash) + node_id
        let token_bytes = crate::noise::token::decode(cluster_token)?;
        let our_hmac = compute_token_verification(&token_bytes, &handshake_hash);

        // Send: hmac (32 bytes) + node_id
        let mut payload = our_hmac.clone();
        payload.extend_from_slice(our_node_id.as_bytes());

        let mut enc_buf = vec![0u8; payload.len() + 16];
        let len = transport
            .write_message(&payload, &mut enc_buf)
            .map_err(|e| NoiseError::Handshake(format!("Token send failed: {}", e)))?;
        send_message(stream, &enc_buf[..len]).await?;

        // Receive peer's token verification
        let msg = recv_message(stream, &mut read_buf).await?;
        let mut dec_buf = vec![0u8; msg.len()];
        let len = transport
            .read_message(&msg, &mut dec_buf)
            .map_err(|e| NoiseError::Handshake(format!("Token recv failed: {}", e)))?;

        if len < 32 {
            return Err(NoiseError::Handshake("Token response too short".into()));
        }

        let peer_hmac = &dec_buf[..32];
        let expected_hmac = compute_token_verification(&token_bytes, &handshake_hash);

        if !verify_token(&expected_hmac, peer_hmac) {
            return Err(NoiseError::TokenVerificationFailed);
        }

        // Extract peer node_id
        let peer_node_id = String::from_utf8_lossy(&dec_buf[32..len]).to_string();

        Ok(NoiseSession {
            transport,
            peer_public_key,
            peer_node_id: Some(peer_node_id),
        })
    }

    // =========================================================================
    // Tests
    // =========================================================================

    #[test]
    fn test_token_verification() {
        let token = b"test_token";
        let hash = b"test_handshake_hash";

        let hmac1 = compute_token_verification(token, hash);
        let hmac2 = compute_token_verification(token, hash);

        assert!(verify_token(&hmac1, &hmac2));

        let wrong_hmac = compute_token_verification(b"wrong_token", hash);
        assert!(!verify_token(&hmac1, &wrong_hmac));
    }

    #[test]
    fn test_token_verification_different_hashes() {
        let token = b"same_token";
        let hash1 = b"hash_one";
        let hash2 = b"hash_two";

        let hmac1 = compute_token_verification(token, hash1);
        let hmac2 = compute_token_verification(token, hash2);

        // Different hashes should produce different HMACs
        assert!(!verify_token(&hmac1, &hmac2));
    }

    #[test]
    fn test_token_verification_empty_inputs() {
        let token = b"";
        let hash = b"";

        let hmac1 = compute_token_verification(token, hash);
        let hmac2 = compute_token_verification(token, hash);

        assert!(verify_token(&hmac1, &hmac2));
    }

    #[tokio::test]
    async fn test_handshake_success() {
        // Generate keypairs
        let server_keypair = NodeKeypair::generate();
        let client_keypair = NodeKeypair::generate();
        let cluster_token = token::generate();

        // Start server
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_keypair_clone = server_keypair.clone();
        let token_clone = cluster_token.clone();

        // Spawn server task
        let server_handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            respond(
                &mut stream,
                &server_keypair_clone,
                &token_clone,
                "server-node",
            )
            .await
        });

        // Client connects
        let mut client_stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let client_result = initiate(
            &mut client_stream,
            &client_keypair,
            &cluster_token,
            "client-node",
        )
        .await;

        // Wait for server
        let server_result = server_handle.await.unwrap();

        // Both should succeed
        assert!(
            client_result.is_ok(),
            "Client handshake failed: {:?}",
            client_result.err()
        );
        assert!(
            server_result.is_ok(),
            "Server handshake failed: {:?}",
            server_result.err()
        );

        let client_session = client_result.unwrap();
        let server_session = server_result.unwrap();

        // Verify peer identities exchanged correctly
        assert_eq!(client_session.peer_node_id(), Some("server-node"));
        assert_eq!(server_session.peer_node_id(), Some("client-node"));

        // Verify both sides got a 32-byte public key from the Noise session
        // Note: These are X25519 keys used by Noise, not the Ed25519 identity keys
        assert_eq!(client_session.peer_public_key().len(), 32);
        assert_eq!(server_session.peer_public_key().len(), 32);

        // The Noise session keys should be non-zero
        assert_ne!(client_session.peer_public_key(), &[0u8; 32]);
        assert_ne!(server_session.peer_public_key(), &[0u8; 32]);
    }

    #[tokio::test]
    async fn test_handshake_wrong_token() {
        let server_keypair = NodeKeypair::generate();
        let client_keypair = NodeKeypair::generate();
        let server_token = token::generate();
        let client_token = token::generate(); // Different token!

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let server_keypair_clone = server_keypair.clone();
        let server_token_clone = server_token.clone();

        let server_handle = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            respond(
                &mut stream,
                &server_keypair_clone,
                &server_token_clone,
                "server",
            )
            .await
        });

        let mut client_stream = tokio::net::TcpStream::connect(addr).await.unwrap();
        let client_result = initiate(
            &mut client_stream,
            &client_keypair,
            &client_token, // Wrong token
            "client",
        )
        .await;

        let server_result = server_handle.await.unwrap();

        // At least one side should fail with token verification error
        let client_failed = client_result.is_err();
        let server_failed = server_result.is_err();

        assert!(
            client_failed || server_failed,
            "Expected handshake to fail with mismatched tokens"
        );
    }
}
