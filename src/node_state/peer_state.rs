//! Peer state types for cluster communication.

use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Per-model stats for a peer node.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerModelStats {
    pub tps: f64,
    pub queue_len: usize,
    /// Learned VRAM usage in MB for this model/profile (0 if unknown)
    #[serde(default)]
    pub vram_mb: u64,
    /// Learned system memory usage in MB for this model/profile (0 if unknown)
    #[serde(default)]
    pub sysmem_mb: u64,
    /// Number of running instances for this model/profile
    #[serde(default)]
    pub instance_count: usize,
}

/// State of a peer node in the cluster.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PeerState {
    pub node_id: String,
    pub address: String,
    pub version: String,
    pub last_seen: u64,
    pub supported_models: Vec<String>,
    #[serde(default)]
    pub model_stats: HashMap<String, PeerModelStats>,
    pub active_instances: usize,
    pub max_instances: usize,
    pub current_requests: u64,
    pub available_vram: u64,
    pub available_sysmem: u64,
    /// Maximum VRAM capacity in MB (for calculating if model fits)
    #[serde(default)]
    pub max_vram: u64,
    /// Maximum system memory capacity in MB
    #[serde(default)]
    pub max_sysmem: u64,
    pub total_queue_length: usize,
    /// Whether this node can serve requests locally (not building, not draining, binary exists)
    #[serde(default = "default_ready")]
    pub ready: bool,
    /// Currently loaded models (model:profile format) - for routing preference
    #[serde(default)]
    pub loaded_models: Vec<String>,
}

fn default_ready() -> bool {
    true // For backwards compatibility with older nodes that don't send this field
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_peer_model_stats_serde_roundtrip() {
        let stats = PeerModelStats {
            tps: 42.5,
            queue_len: 3,
            vram_mb: 8000,
            sysmem_mb: 16000,
            instance_count: 2,
        };
        let json = serde_json::to_string(&stats).unwrap();
        let parsed: PeerModelStats = serde_json::from_str(&json).unwrap();
        assert!((parsed.tps - 42.5).abs() < 0.001);
        assert_eq!(parsed.queue_len, 3);
        assert_eq!(parsed.vram_mb, 8000);
        assert_eq!(parsed.sysmem_mb, 16000);
        assert_eq!(parsed.instance_count, 2);
    }

    #[test]
    fn test_peer_model_stats_backwards_compat() {
        // Old nodes don't send the new fields - should default to 0
        let json = r#"{"tps": 10.0, "queue_len": 1}"#;
        let parsed: PeerModelStats = serde_json::from_str(json).unwrap();
        assert_eq!(parsed.vram_mb, 0);
        assert_eq!(parsed.sysmem_mb, 0);
        assert_eq!(parsed.instance_count, 0);
    }

    #[test]
    fn test_peer_state_serde_roundtrip() {
        let state = PeerState {
            node_id: "node-1".into(),
            address: "http://localhost:8080".into(),
            version: "1.0.0".into(),
            last_seen: 12345,
            supported_models: vec!["model:fast".into()],
            model_stats: HashMap::new(),
            active_instances: 2,
            max_instances: 10,
            current_requests: 5,
            available_vram: 1000,
            available_sysmem: 2000,
            max_vram: 16000,
            max_sysmem: 64000,
            total_queue_length: 3,
            ready: true,
            loaded_models: vec!["model:fast".into()],
        };
        let json = serde_json::to_string(&state).unwrap();
        let parsed: PeerState = serde_json::from_str(&json).unwrap();
        assert_eq!(parsed.node_id, "node-1");
        assert_eq!(parsed.address, "http://localhost:8080");
        assert_eq!(parsed.active_instances, 2);
        assert_eq!(parsed.max_vram, 16000);
        assert_eq!(parsed.max_sysmem, 64000);
        assert!(parsed.ready);
        assert_eq!(parsed.loaded_models, vec!["model:fast"]);
    }

    #[test]
    fn test_peer_state_default_ready() {
        // Test backwards compatibility - missing "ready" field defaults to true
        let json = r#"{
            "node_id": "node-old",
            "address": "http://old:8080",
            "version": "0.0.1",
            "last_seen": 100,
            "supported_models": [],
            "active_instances": 0,
            "max_instances": 1,
            "current_requests": 0,
            "available_vram": 0,
            "available_sysmem": 0,
            "total_queue_length": 0
        }"#;
        let parsed: PeerState = serde_json::from_str(json).unwrap();
        assert!(parsed.ready); // Should default to true
    }

    #[test]
    fn test_default_ready_returns_true() {
        assert!(default_ready());
    }
}
