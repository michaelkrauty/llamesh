//! Cluster token management.
//!
//! The cluster token is a shared secret that proves cluster membership.
//! Used during Noise handshake as additional authentication.
//!
//! Storage: ~/.llama-mesh/cluster_token (mode 600)
//! Env override: CLUSTER_TOKEN

use super::permissions::{self, FILE_MODE};
use super::{NoiseError, Result};
use base64::{engine::general_purpose::STANDARD as BASE64, Engine};
use std::fs;
use std::path::Path;

/// Token length in bytes (256 bits)
const TOKEN_LENGTH: usize = 32;

/// Generate a new random cluster token
pub fn generate() -> String {
    let mut bytes = [0u8; TOKEN_LENGTH];
    getrandom::getrandom(&mut bytes).expect("Failed to generate random token");
    BASE64.encode(bytes)
}

/// Save token to file
pub fn save(path: &Path, token: &str) -> Result<()> {
    fs::write(path, token)?;
    permissions::secure_file(path)?;
    tracing::info!(
        path = %path.display(),
        "Saved cluster token"
    );
    Ok(())
}

/// Load token from file
pub fn load(path: &Path) -> Result<String> {
    // Check permissions first
    permissions::check_file_mode(path, FILE_MODE)?;

    let token = fs::read_to_string(path)?.trim().to_string();

    if token.is_empty() {
        return Err(NoiseError::Token("Token file is empty".into()));
    }

    // Validate base64
    BASE64
        .decode(&token)
        .map_err(|e| NoiseError::Token(format!("Invalid token format: {}", e)))?;

    Ok(token)
}

/// Get or create cluster token.
/// Checks env var CLUSTER_TOKEN first, then file.
pub fn get_or_create(config_dir: &Path) -> Result<String> {
    // Check env var override first
    if let Ok(env_token) = std::env::var("CLUSTER_TOKEN") {
        let token = env_token.trim().to_string();
        if !token.is_empty() {
            // Validate
            BASE64
                .decode(&token)
                .map_err(|e| NoiseError::Token(format!("Invalid CLUSTER_TOKEN env var: {}", e)))?;
            tracing::info!("Using cluster token from CLUSTER_TOKEN env var");
            return Ok(token);
        }
    }

    let token_path = config_dir.join("cluster_token");

    if token_path.exists() {
        let token = load(&token_path)?;
        tracing::info!(
            path = %token_path.display(),
            "Loaded cluster token"
        );
        Ok(token)
    } else {
        let token = generate();
        save(&token_path, &token)?;
        tracing::warn!(
            path = %token_path.display(),
            "Generated new cluster token - copy this to other nodes in the cluster"
        );
        Ok(token)
    }
}

/// Decode token to bytes (for HMAC operations)
pub fn decode(token: &str) -> Result<Vec<u8>> {
    BASE64
        .decode(token)
        .map_err(|e| NoiseError::Token(format!("Invalid token: {}", e)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    #[test]
    fn test_generate_valid_base64() {
        let token = generate();
        assert!(BASE64.decode(&token).is_ok());
        assert_eq!(BASE64.decode(&token).unwrap().len(), TOKEN_LENGTH);
    }

    #[test]
    fn test_save_and_load() {
        let dir = tempdir().unwrap();
        let token_path = dir.path().join("cluster_token");

        let token = generate();
        save(&token_path, &token).unwrap();

        let loaded = load(&token_path).unwrap();
        assert_eq!(token, loaded);
    }

    #[test]
    fn test_load_checks_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = tempdir().unwrap();
        let token_path = dir.path().join("cluster_token");

        let token = generate();
        fs::write(&token_path, &token).unwrap();
        fs::set_permissions(&token_path, fs::Permissions::from_mode(0o644)).unwrap();

        // Should fail due to wrong permissions
        assert!(load(&token_path).is_err());
    }

    #[test]
    fn test_decode() {
        let token = generate();
        let bytes = decode(&token).unwrap();
        assert_eq!(bytes.len(), TOKEN_LENGTH);
    }
}
