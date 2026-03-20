//! Known peers management with TOFU (Trust-On-First-Use) support.
//!
//! File format (~/.llama-mesh/known_peers):
//! ```text
//! # node_id pubkey first_seen
//! node-b ed25519:xAbC123...= 2024-01-15T10:30:00Z
//! ```

use super::permissions::{self, FILE_MODE};
use super::{NoiseError, Result};
use chrono::{DateTime, Utc};
use parking_lot::RwLock;
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A known peer entry
#[derive(Debug, Clone)]
pub struct KnownPeer {
    pub node_id: String,
    pub public_key: String,
    pub first_seen: DateTime<Utc>,
}

/// Trust decision for a peer
#[derive(Debug, Clone, PartialEq)]
pub enum TrustDecision {
    /// Peer is already known and key matches
    Trusted,
    /// New peer, accepted via TOFU
    TofuAccepted,
    /// New peer, but TOFU disabled and not in allowed_keys
    Rejected,
    /// Peer is known but presented different key
    KeyMismatch { known: String },
}

/// Known peers store with file persistence
#[derive(Clone)]
pub struct KnownPeers {
    peers: Arc<RwLock<HashMap<String, KnownPeer>>>,
    path: PathBuf,
}

impl KnownPeers {
    /// Load known peers from file, or create empty store
    pub fn load_or_create(path: &Path) -> Result<Self> {
        let peers = if path.exists() {
            permissions::check_file_mode(path, FILE_MODE)?;
            Self::parse_file(path)?
        } else {
            HashMap::new()
        };

        Ok(Self {
            peers: Arc::new(RwLock::new(peers)),
            path: path.to_path_buf(),
        })
    }

    /// Parse known peers file
    fn parse_file(path: &Path) -> Result<HashMap<String, KnownPeer>> {
        let content = fs::read_to_string(path)?;
        let mut peers = HashMap::new();

        for line in content.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }

            let parts: Vec<&str> = line.split_whitespace().collect();
            if parts.len() < 3 {
                tracing::warn!(line = %line, "Invalid known_peers line, skipping");
                continue;
            }

            let node_id = parts[0].to_string();
            let public_key = parts[1].to_string();
            let first_seen = parts[2]
                .parse::<DateTime<Utc>>()
                .unwrap_or_else(|_| Utc::now());

            peers.insert(
                node_id.clone(),
                KnownPeer {
                    node_id,
                    public_key,
                    first_seen,
                },
            );
        }

        Ok(peers)
    }

    /// Save known peers to file
    fn save(&self) -> Result<()> {
        let peers = self.peers.read();
        let mut content = String::from("# node_id pubkey first_seen\n");

        for peer in peers.values() {
            content.push_str(&format!(
                "{} {} {}\n",
                peer.node_id,
                peer.public_key,
                peer.first_seen.to_rfc3339()
            ));
        }

        fs::write(&self.path, &content)?;
        permissions::secure_file(&self.path)?;
        Ok(())
    }

    /// Check if a peer should be trusted.
    /// Returns trust decision based on TOFU policy and allowed_keys.
    pub fn check_peer(
        &self,
        node_id: &str,
        public_key: &str,
        tofu_enabled: bool,
        allowed_keys: &[String],
    ) -> TrustDecision {
        let peers = self.peers.read();

        // Check if peer is already known
        if let Some(known) = peers.get(node_id) {
            if known.public_key == public_key {
                return TrustDecision::Trusted;
            } else {
                return TrustDecision::KeyMismatch {
                    known: known.public_key.clone(),
                };
            }
        }

        // Peer is new - check allowed_keys first (enterprise mode)
        if !allowed_keys.is_empty() {
            if allowed_keys.iter().any(|k| k == public_key) {
                return TrustDecision::TofuAccepted; // Allowed by pinning
            } else {
                return TrustDecision::Rejected;
            }
        }

        // No allowed_keys - use TOFU
        if tofu_enabled {
            TrustDecision::TofuAccepted
        } else {
            TrustDecision::Rejected
        }
    }

    /// Add a new peer (after TOFU acceptance)
    pub fn add_peer(&self, node_id: &str, public_key: &str) -> Result<()> {
        let peer = KnownPeer {
            node_id: node_id.to_string(),
            public_key: public_key.to_string(),
            first_seen: Utc::now(),
        };

        {
            let mut peers = self.peers.write();
            peers.insert(node_id.to_string(), peer);
        }

        self.save()?;

        tracing::warn!(
            node_id = %node_id,
            public_key = %public_key,
            "TOFU: Accepted new peer"
        );

        Ok(())
    }

    /// Get a known peer by node_id
    #[allow(dead_code)] // Reserved for peer management CLI
    pub fn get(&self, node_id: &str) -> Option<KnownPeer> {
        self.peers.read().get(node_id).cloned()
    }

    /// List all known peers
    #[allow(dead_code)] // Reserved for peer management CLI
    pub fn list(&self) -> Vec<KnownPeer> {
        self.peers.read().values().cloned().collect()
    }

    /// Remove a peer
    #[allow(dead_code)] // Reserved for peer management CLI
    pub fn remove(&self, node_id: &str) -> Result<bool> {
        let removed = {
            let mut peers = self.peers.write();
            peers.remove(node_id).is_some()
        };

        if removed {
            self.save()?;
        }

        Ok(removed)
    }
}

/// Verify a peer and handle trust decision
pub fn verify_peer(
    known_peers: &KnownPeers,
    node_id: &str,
    public_key: &str,
    config: &crate::config::ClusterNoiseConfig,
) -> Result<()> {
    let decision = known_peers.check_peer(node_id, public_key, config.tofu, &config.allowed_keys);

    match decision {
        TrustDecision::Trusted => {
            tracing::debug!(node_id = %node_id, "Peer verified (known)");
            Ok(())
        }
        TrustDecision::TofuAccepted => {
            known_peers.add_peer(node_id, public_key)?;
            Ok(())
        }
        TrustDecision::Rejected => Err(NoiseError::PeerNotAllowed {
            node_id: node_id.to_string(),
            key: public_key.to_string(),
        }),
        TrustDecision::KeyMismatch { known } => {
            tracing::error!(
                node_id = %node_id,
                presented = %public_key,
                known = %known,
                "SECURITY: Peer key mismatch - possible MITM attack"
            );
            Err(NoiseError::KeyMismatch {
                node_id: node_id.to_string(),
                presented: public_key.to_string(),
                known,
            })
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn setup() -> (tempfile::TempDir, KnownPeers) {
        let dir = tempdir().unwrap();
        let path = dir.path().join("known_peers");
        let kp = KnownPeers::load_or_create(&path).unwrap();
        (dir, kp)
    }

    #[test]
    fn test_new_peer_tofu_enabled() {
        let (_dir, kp) = setup();
        let decision = kp.check_peer("node-b", "ed25519:abc123", true, &[]);
        assert_eq!(decision, TrustDecision::TofuAccepted);
    }

    #[test]
    fn test_new_peer_tofu_disabled() {
        let (_dir, kp) = setup();
        let decision = kp.check_peer("node-b", "ed25519:abc123", false, &[]);
        assert_eq!(decision, TrustDecision::Rejected);
    }

    #[test]
    fn test_known_peer_trusted() {
        let (_dir, kp) = setup();
        kp.add_peer("node-b", "ed25519:abc123").unwrap();

        let decision = kp.check_peer("node-b", "ed25519:abc123", true, &[]);
        assert_eq!(decision, TrustDecision::Trusted);
    }

    #[test]
    fn test_key_mismatch() {
        let (_dir, kp) = setup();
        kp.add_peer("node-b", "ed25519:abc123").unwrap();

        let decision = kp.check_peer("node-b", "ed25519:different", true, &[]);
        assert!(matches!(decision, TrustDecision::KeyMismatch { .. }));
    }

    #[test]
    fn test_allowed_keys_accepts() {
        let (_dir, kp) = setup();
        let allowed = vec!["ed25519:abc123".to_string()];
        let decision = kp.check_peer("node-b", "ed25519:abc123", false, &allowed);
        assert_eq!(decision, TrustDecision::TofuAccepted);
    }

    #[test]
    fn test_allowed_keys_rejects() {
        let (_dir, kp) = setup();
        let allowed = vec!["ed25519:other".to_string()];
        let decision = kp.check_peer("node-b", "ed25519:abc123", false, &allowed);
        assert_eq!(decision, TrustDecision::Rejected);
    }

    #[test]
    fn test_persistence() {
        let dir = tempdir().unwrap();
        let path = dir.path().join("known_peers");

        // Add peer
        {
            let kp = KnownPeers::load_or_create(&path).unwrap();
            kp.add_peer("node-b", "ed25519:abc123").unwrap();
        }

        // Reload and verify
        {
            let kp = KnownPeers::load_or_create(&path).unwrap();
            let peer = kp.get("node-b").unwrap();
            assert_eq!(peer.public_key, "ed25519:abc123");
        }
    }
}
