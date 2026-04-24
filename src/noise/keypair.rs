//! Noise static key management for node identity.
//!
//! Keys are stored in ~/.llama-mesh/node.key with mode 600.
//! Supports env var override via NODE_PRIVATE_KEY (base64-encoded).

use super::permissions::{self, FILE_MODE};
use super::{NoiseError, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use std::fs;
use std::path::Path;
use x25519_dalek::{PublicKey, StaticSecret};

/// Node static key wrapper for Noise_XX_25519.
#[derive(Clone)]
pub struct NodeKeypair {
    private_key: [u8; 32],
}

impl NodeKeypair {
    /// Generate a new random static keypair.
    pub fn generate() -> Self {
        let mut private_key = [0u8; 32];
        getrandom::getrandom(&mut private_key).expect("Failed to generate random key");
        Self { private_key }
    }

    /// Load keypair from raw 32-byte Noise static private key material.
    pub fn from_seed(seed: &[u8; 32]) -> Self {
        Self { private_key: *seed }
    }

    /// Get private key bytes for the Noise handshake.
    pub fn private_key_bytes(&self) -> [u8; 32] {
        self.private_key
    }

    /// Get the X25519 public key bytes used by the Noise handshake.
    pub fn public_key_bytes(&self) -> [u8; 32] {
        let secret = StaticSecret::from(self.private_key);
        PublicKey::from(&secret).to_bytes()
    }

    /// Get public key in display format: noise25519:base64...
    pub fn public_key_display(&self) -> String {
        format!("noise25519:{}", BASE64.encode(self.public_key_bytes()))
    }

    /// Parse a public key from display format
    #[allow(dead_code)] // Used in tests, reserved for key management CLI
    pub fn parse_public_key(s: &str) -> Result<[u8; 32]> {
        let s = s
            .strip_prefix("noise25519:")
            .or_else(|| s.strip_prefix("ed25519:"))
            .unwrap_or(s);
        let bytes = BASE64
            .decode(s)
            .map_err(|e| NoiseError::KeyFile(format!("Invalid base64: {e}")))?;
        bytes
            .try_into()
            .map_err(|_| NoiseError::KeyFile("Public key must be 32 bytes".into()))
    }

    /// Save keypair to file
    pub fn save(&self, path: &Path) -> Result<()> {
        let encoded = BASE64.encode(self.private_key_bytes());
        fs::write(path, encoded)?;
        permissions::secure_file(path)?;
        tracing::info!(
            path = %path.display(),
            pubkey = %self.public_key_display(),
            "Saved node keypair"
        );
        Ok(())
    }

    /// Load keypair from file
    pub fn load(path: &Path) -> Result<Self> {
        // Check permissions first
        permissions::check_file_mode(path, FILE_MODE)?;

        let encoded = fs::read_to_string(path)?;
        let bytes = BASE64
            .decode(encoded.trim())
            .map_err(|e| NoiseError::KeyFile(format!("Invalid base64 in key file: {e}")))?;

        let private_key: [u8; 32] = bytes
            .try_into()
            .map_err(|_| NoiseError::KeyFile("Key file must contain 32 bytes".into()))?;

        Ok(Self::from_seed(&private_key))
    }
}

/// Get or create node keypair.
/// Checks env var NODE_PRIVATE_KEY first, then file.
pub fn get_or_create(
    config_dir: &Path,
    config: &crate::config::ClusterNoiseConfig,
) -> Result<NodeKeypair> {
    // Check env var override first
    if let Ok(env_key) = std::env::var("NODE_PRIVATE_KEY") {
        let bytes = BASE64
            .decode(env_key.trim())
            .map_err(|e| NoiseError::KeyFile(format!("Invalid NODE_PRIVATE_KEY env var: {e}")))?;
        let private_key: [u8; 32] = bytes
            .try_into()
            .map_err(|_| NoiseError::KeyFile("NODE_PRIVATE_KEY must be 32 bytes base64".into()))?;
        tracing::info!("Using node keypair from NODE_PRIVATE_KEY env var");
        return Ok(NodeKeypair::from_seed(&private_key));
    }

    let key_path = config.effective_private_key_path(config_dir);

    if key_path.exists() {
        let keypair = NodeKeypair::load(&key_path)?;
        tracing::info!(
            path = %key_path.display(),
            pubkey = %keypair.public_key_display(),
            "Loaded node keypair"
        );
        Ok(keypair)
    } else {
        let keypair = NodeKeypair::generate();
        keypair.save(&key_path)?;
        Ok(keypair)
    }
}

/// Load previous keypair for key rotation (optional)
#[allow(dead_code)] // Key rotation not yet implemented
pub fn load_previous(config: &crate::config::ClusterNoiseConfig) -> Result<Option<NodeKeypair>> {
    match &config.previous_key_path {
        Some(path) => {
            let path = Path::new(path);
            if path.exists() {
                let keypair = NodeKeypair::load(path)?;
                tracing::info!(
                    path = %path.display(),
                    pubkey = %keypair.public_key_display(),
                    "Loaded previous keypair for rotation"
                );
                Ok(Some(keypair))
            } else {
                Ok(None)
            }
        }
        None => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use tempfile::tempdir;

    #[test]
    fn test_generate_and_save_load() {
        let dir = tempdir().unwrap();
        let key_path = dir.path().join("node.key");

        let keypair1 = NodeKeypair::generate();
        keypair1.save(&key_path).unwrap();

        let keypair2 = NodeKeypair::load(&key_path).unwrap();

        assert_eq!(keypair1.public_key_bytes(), keypair2.public_key_bytes());
        assert_eq!(keypair1.private_key_bytes(), keypair2.private_key_bytes());
    }

    #[test]
    fn test_public_key_display_roundtrip() {
        let keypair = NodeKeypair::generate();
        let display = keypair.public_key_display();
        let parsed = NodeKeypair::parse_public_key(&display).unwrap();
        assert_eq!(keypair.public_key_bytes(), parsed);
    }

    #[test]
    fn test_from_seed_deterministic() {
        let seed = [42u8; 32];
        let keypair1 = NodeKeypair::from_seed(&seed);
        let keypair2 = NodeKeypair::from_seed(&seed);
        assert_eq!(keypair1.public_key_bytes(), keypair2.public_key_bytes());
    }

    #[test]
    fn test_generate_unique_keys() {
        let keypair1 = NodeKeypair::generate();
        let keypair2 = NodeKeypair::generate();
        // Each generate() should produce different keys
        assert_ne!(keypair1.public_key_bytes(), keypair2.public_key_bytes());
    }

    #[test]
    fn test_parse_public_key_without_prefix() {
        let keypair = NodeKeypair::generate();
        // Get the base64 part without prefix
        let display = keypair.public_key_display();
        let base64_only = display.strip_prefix("noise25519:").unwrap();

        // Should still parse correctly
        let parsed = NodeKeypair::parse_public_key(base64_only).unwrap();
        assert_eq!(keypair.public_key_bytes(), parsed);
    }

    #[test]
    fn test_parse_public_key_invalid_base64() {
        let result = NodeKeypair::parse_public_key("not-valid-base64!!!");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_public_key_wrong_length() {
        // Valid base64 but wrong length (only 16 bytes)
        let result = NodeKeypair::parse_public_key("noise25519:AAAAAAAAAAAAAAAAAAAAAA==");
        assert!(result.is_err());
    }

    #[test]
    fn test_parse_public_key_legacy_ed25519_prefix() {
        let keypair = NodeKeypair::generate();
        let legacy = format!("ed25519:{}", BASE64.encode(keypair.public_key_bytes()));
        let parsed = NodeKeypair::parse_public_key(&legacy).unwrap();
        assert_eq!(keypair.public_key_bytes(), parsed);
    }

    #[test]
    fn test_load_wrong_permissions() {
        let dir = tempdir().unwrap();
        let key_path = dir.path().join("node.key");

        let keypair = NodeKeypair::generate();
        keypair.save(&key_path).unwrap();

        // Change permissions to world-readable (insecure)
        fs::set_permissions(&key_path, fs::Permissions::from_mode(0o644)).unwrap();

        // Should fail to load
        let result = NodeKeypair::load(&key_path);
        assert!(result.is_err());
    }

    #[test]
    fn test_save_sets_correct_permissions() {
        let dir = tempdir().unwrap();
        let key_path = dir.path().join("node.key");

        let keypair = NodeKeypair::generate();
        keypair.save(&key_path).unwrap();

        let metadata = fs::metadata(&key_path).unwrap();
        let mode = metadata.permissions().mode() & 0o777;
        assert_eq!(mode, 0o600, "Key file should have mode 600");
    }

    #[test]
    fn test_private_key_bytes_length() {
        let keypair = NodeKeypair::generate();
        assert_eq!(keypair.private_key_bytes().len(), 32);
    }

    #[test]
    fn test_public_key_bytes_length() {
        let keypair = NodeKeypair::generate();
        assert_eq!(keypair.public_key_bytes().len(), 32);
    }
}
