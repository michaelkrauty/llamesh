//! Noise Protocol implementation for secure inter-node communication.
//!
//! Cluster gossip and peer forwarding use encrypted Noise transport when enabled.
//!
//! Uses Noise_XX_25519_ChaChaPoly_SHA256 for:
//! - Mutual authentication via Noise static X25519 keys
//! - Perfect forward secrecy
//! - Zero-config encryption (auto-generated keys)
//!
//! Trust model:
//! - TOFU (Trust-On-First-Use) for hobbyist setups
//! - Key pinning via `allowed_keys` for enterprise

pub mod handshake;
pub mod keypair;
pub mod known_peers;
pub mod permissions;
pub mod token;
pub mod transport;

use thiserror::Error;

#[derive(Debug, Error)]
pub enum NoiseError {
    #[error("Permission denied: {path} has mode {actual:o}, expected {expected:o}")]
    PermissionDenied {
        path: String,
        actual: u32,
        expected: u32,
    },

    #[allow(dead_code)] // Reserved for future error handling paths
    #[error("Config directory not found and could not be created: {0}")]
    ConfigDirCreation(String),

    #[error("Key file error: {0}")]
    KeyFile(String),

    #[error("Token error: {0}")]
    Token(String),

    #[error("Handshake failed: {0}")]
    Handshake(String),

    #[error("Peer key mismatch: {node_id} presented key {presented} but known key is {known}")]
    KeyMismatch {
        node_id: String,
        presented: String,
        known: String,
    },

    #[error("Peer not allowed: {node_id} with key {key} is not in allowed_keys")]
    PeerNotAllowed { node_id: String, key: String },

    #[error("Token verification failed")]
    TokenVerificationFailed,

    #[error("Transport error: {0}")]
    Transport(String),

    #[error("IO error: {0}")]
    Io(#[from] std::io::Error),

    #[error("Snow error: {0}")]
    Snow(#[from] snow::Error),
}

pub type Result<T> = std::result::Result<T, NoiseError>;

/// Default config directory path (~/.llama-mesh/)
pub fn default_config_dir() -> std::path::PathBuf {
    dirs::home_dir()
        .map(|h| h.join(".llama-mesh"))
        .unwrap_or_else(|| {
            tracing::warn!("Could not determine home directory; using ./.llama-mesh for config");
            std::path::PathBuf::from("./.llama-mesh")
        })
}

/// Initialize the noise subsystem.
/// Creates config directory and generates keys/token if needed.
pub async fn initialize(
    config: &crate::config::ClusterNoiseConfig,
    node_id: String,
) -> Result<NoiseContext> {
    let config_dir = config.effective_config_dir();

    // Ensure config directory exists with correct permissions
    permissions::ensure_config_dir(&config_dir)?;

    // Load or generate keypair
    let keypair = keypair::get_or_create(&config_dir, config)?;

    // Load or generate token
    let cluster_token = token::get_or_create(&config_dir)?;

    // Load known peers
    let known_peers_path = config.effective_known_peers_path(&config_dir);
    let known_peers = known_peers::KnownPeers::load_or_create(&known_peers_path)?;

    Ok(NoiseContext {
        keypair,
        cluster_token,
        known_peers,
        config: config.clone(),
        node_id,
    })
}

/// Runtime context for noise protocol operations
#[derive(Clone)]
pub struct NoiseContext {
    pub keypair: keypair::NodeKeypair,
    pub cluster_token: String,
    pub known_peers: known_peers::KnownPeers,
    pub config: crate::config::ClusterNoiseConfig,
    pub node_id: String,
}

impl NoiseContext {
    /// Get our public key in display format (noise25519:base64...)
    pub fn public_key_display(&self) -> String {
        self.keypair.public_key_display()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_default_config_dir_returns_path() {
        let dir = default_config_dir();
        // Should end with .llama-mesh
        assert!(dir.to_string_lossy().ends_with(".llama-mesh"));
    }

    #[test]
    fn test_noise_error_display_permission_denied() {
        let err = NoiseError::PermissionDenied {
            path: "/some/path".into(),
            actual: 0o644,
            expected: 0o600,
        };
        let msg = format!("{err}");
        assert!(msg.contains("Permission denied"));
        assert!(msg.contains("/some/path"));
    }

    #[test]
    fn test_noise_error_display_key_mismatch() {
        let err = NoiseError::KeyMismatch {
            node_id: "node-1".into(),
            presented: "key-a".into(),
            known: "key-b".into(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("node-1"));
        assert!(msg.contains("key-a"));
        assert!(msg.contains("key-b"));
    }

    #[test]
    fn test_noise_error_display_token_verification() {
        let err = NoiseError::TokenVerificationFailed;
        let msg = format!("{err}");
        assert!(msg.contains("Token verification failed"));
    }

    #[test]
    fn test_noise_error_display_peer_not_allowed() {
        let err = NoiseError::PeerNotAllowed {
            node_id: "bad-node".into(),
            key: "bad-key".into(),
        };
        let msg = format!("{err}");
        assert!(msg.contains("bad-node"));
        assert!(msg.contains("not in allowed_keys"));
    }
}
