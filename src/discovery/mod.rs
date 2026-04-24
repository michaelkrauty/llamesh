//!
//! Supports:
//! - mDNS for zero-config LAN discovery
//! - Explicit peer list for WAN/cross-subnet
//!
//! Both methods are additive - discovered peers are merged.

pub mod mdns;

use parking_lot::RwLock;
use std::collections::HashSet;
use std::sync::Arc;

/// Discovered peer info
#[derive(Debug, Clone, Hash, Eq, PartialEq)]
pub struct DiscoveredPeer {
    pub node_id: String,
    pub cluster_addr: String,
    pub public_key: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_discovered_peer_equality() {
        let peer1 = DiscoveredPeer {
            node_id: "node1".to_string(),
            cluster_addr: "127.0.0.1:8080".to_string(),
            public_key: Some("key1".to_string()),
        };
        let peer2 = DiscoveredPeer {
            node_id: "node1".to_string(),
            cluster_addr: "127.0.0.1:8080".to_string(),
            public_key: Some("key1".to_string()),
        };
        assert_eq!(peer1, peer2);
    }

    #[test]
    fn test_discovered_peer_inequality() {
        let peer1 = DiscoveredPeer {
            node_id: "node1".to_string(),
            cluster_addr: "127.0.0.1:8080".to_string(),
            public_key: None,
        };
        let peer2 = DiscoveredPeer {
            node_id: "node2".to_string(),
            cluster_addr: "127.0.0.1:8080".to_string(),
            public_key: None,
        };
        assert_ne!(peer1, peer2);
    }

    #[test]
    fn test_discovered_peer_hash() {
        let peer1 = DiscoveredPeer {
            node_id: "node1".to_string(),
            cluster_addr: "127.0.0.1:8080".to_string(),
            public_key: None,
        };
        let peer2 = peer1.clone();

        let mut set = HashSet::new();
        set.insert(peer1);
        // Inserting duplicate should not increase size
        set.insert(peer2);
        assert_eq!(set.len(), 1);
    }

    #[test]
    fn test_discovered_peer_clone() {
        let peer = DiscoveredPeer {
            node_id: "node1".to_string(),
            cluster_addr: "127.0.0.1:8080".to_string(),
            public_key: Some("pubkey".to_string()),
        };
        let cloned = peer.clone();
        assert_eq!(peer.node_id, cloned.node_id);
        assert_eq!(peer.cluster_addr, cloned.cluster_addr);
        assert_eq!(peer.public_key, cloned.public_key);
    }

    #[test]
    fn test_discovered_peer_debug() {
        let peer = DiscoveredPeer {
            node_id: "test-node".to_string(),
            cluster_addr: "10.0.0.1:9000".to_string(),
            public_key: Some("abc123".to_string()),
        };
        let debug_str = format!("{peer:?}");
        assert!(debug_str.contains("test-node"));
        assert!(debug_str.contains("10.0.0.1:9000"));
        assert!(debug_str.contains("abc123"));
    }

    #[test]
    fn test_discovered_peer_hashset_operations() {
        let mut peers: HashSet<DiscoveredPeer> = HashSet::new();

        let peer1 = DiscoveredPeer {
            node_id: "node1".to_string(),
            cluster_addr: "127.0.0.1:8080".to_string(),
            public_key: None,
        };
        let peer2 = DiscoveredPeer {
            node_id: "node2".to_string(),
            cluster_addr: "127.0.0.1:8081".to_string(),
            public_key: Some("key2".to_string()),
        };

        peers.insert(peer1.clone());
        peers.insert(peer2.clone());
        assert_eq!(peers.len(), 2);

        // Remove by retain
        peers.retain(|p| p.node_id != "node1");
        assert_eq!(peers.len(), 1);
        assert!(peers.contains(&peer2));
    }

    #[test]
    fn test_discovered_peer_public_key_none() {
        let peer = DiscoveredPeer {
            node_id: "node".to_string(),
            cluster_addr: "addr".to_string(),
            public_key: None,
        };
        assert!(peer.public_key.is_none());
    }
}

/// Peer discovery manager
pub struct Discovery {
    /// All discovered peers (from any source)
    peers: Arc<RwLock<HashSet<DiscoveredPeer>>>,
    /// Explicit peers from config (stored for potential re-add after removal)
    #[allow(dead_code)]
    explicit_peers: Vec<String>,
    /// mDNS handle - held to keep background discovery alive
    #[allow(dead_code)]
    mdns: Option<mdns::MdnsDiscovery>,
}

impl Discovery {
    /// Create a new discovery manager
    pub fn new(
        config: &crate::config::ClusterConfig,
        node_id: &str,
        listen_port: u16,
        public_key: &str,
    ) -> anyhow::Result<Self> {
        let peers = Arc::new(RwLock::new(HashSet::new()));

        // Add explicit peers
        for peer_addr in &config.peers {
            peers.write().insert(DiscoveredPeer {
                node_id: format!("explicit:{peer_addr}"),
                cluster_addr: peer_addr.clone(),
                public_key: None,
            });
        }

        // Start mDNS if enabled
        let mdns = if config.discovery.mdns {
            Some(mdns::MdnsDiscovery::new(
                &config.discovery.service_name,
                node_id,
                listen_port,
                public_key,
                peers.clone(),
            )?)
        } else {
            None
        };

        Ok(Self {
            peers,
            explicit_peers: config.peers.clone(),
            mdns,
        })
    }

    /// Get all currently discovered peers
    #[allow(dead_code)] // Reserved for peer management API
    pub fn get_peers(&self) -> Vec<DiscoveredPeer> {
        self.peers.read().iter().cloned().collect()
    }

    /// Get just the cluster addresses
    pub fn get_peer_addrs(&self) -> Vec<String> {
        self.peers
            .read()
            .iter()
            .map(|p| p.cluster_addr.clone())
            .collect()
    }

    /// Add an explicit peer
    #[allow(dead_code)] // Reserved for peer management API
    pub fn add_peer(&self, addr: &str) {
        self.peers.write().insert(DiscoveredPeer {
            node_id: format!("explicit:{addr}"),
            cluster_addr: addr.to_string(),
            public_key: None,
        });
    }

    /// Remove a peer
    #[allow(dead_code)] // Reserved for peer management API
    pub fn remove_peer(&self, addr: &str) {
        self.peers.write().retain(|p| p.cluster_addr != addr);
    }

    /// Shutdown discovery
    #[allow(dead_code)] // Reserved for graceful shutdown API
    pub fn shutdown(&mut self) {
        if let Some(mdns) = self.mdns.take() {
            mdns.shutdown();
        }
    }
}
