//! mDNS-based peer discovery for zero-config LAN operation.
//!
//! Announces and discovers peers on local network.
//! Service type: _llama-mesh._tcp.local
//! TXT records: node_id, pubkey

use super::DiscoveredPeer;
use mdns_sd::{ServiceDaemon, ServiceEvent, ServiceInfo};
use parking_lot::RwLock;
use std::collections::HashSet;
use std::sync::Arc;
use std::thread;

/// mDNS discovery handler
pub struct MdnsDiscovery {
    /// Daemon handle - held to keep mDNS service alive (Drop stops it)
    #[allow(dead_code)]
    daemon: ServiceDaemon,
    /// Service name for unregistration
    #[allow(dead_code)]
    service_name: String,
    /// Shutdown coordination flag
    #[allow(dead_code)]
    shutdown_flag: Arc<RwLock<bool>>,
}

impl MdnsDiscovery {
    /// Create and start mDNS discovery
    pub fn new(
        service_type: &str,
        node_id: &str,
        listen_port: u16,
        public_key: &str,
        peers: Arc<RwLock<HashSet<DiscoveredPeer>>>,
    ) -> anyhow::Result<Self> {
        let daemon = ServiceDaemon::new()?;

        // Normalize service type to end with ".local." as required by mdns-sd
        // Input might be "_llama-mesh._tcp.local" or "_llama-mesh._tcp"
        let service_type = service_type.trim_end_matches('.');
        let service_type = service_type.trim_end_matches(".local");
        let service_type = format!("{service_type}.local.");

        // Get hostname and ensure it ends with .local. as required by mdns-sd
        let hostname = hostname::get()
            .map(|h| h.to_string_lossy().to_string())
            .unwrap_or_else(|_| "unknown".to_string());
        let hostname = if hostname.ends_with(".local.") {
            hostname
        } else if hostname.ends_with(".local") {
            format!("{hostname}.")
        } else {
            format!("{hostname}.local.")
        };

        // Create service info
        let properties = vec![
            ("node_id".to_string(), node_id.to_string()),
            ("pubkey".to_string(), public_key.to_string()),
        ];

        let service_info = ServiceInfo::new(
            &service_type,
            node_id,
            &hostname,
            (),
            listen_port,
            properties.as_slice(),
        )?;

        // Register our service
        daemon.register(service_info)?;

        tracing::info!(
            service_type = %service_type,
            node_id = %node_id,
            port = listen_port,
            "Registered mDNS service"
        );

        // Browse for peers
        let receiver = daemon.browse(&service_type)?;
        let shutdown_flag = Arc::new(RwLock::new(false));
        let shutdown_flag_clone = shutdown_flag.clone();
        let service_type_clone = service_type.clone();

        thread::spawn(move || {
            Self::browse_loop(receiver, peers, shutdown_flag_clone, &service_type_clone);
        });

        Ok(Self {
            daemon,
            service_name: service_type,
            shutdown_flag,
        })
    }

    /// Background loop to process mDNS events
    fn browse_loop(
        receiver: mdns_sd::Receiver<ServiceEvent>,
        peers: Arc<RwLock<HashSet<DiscoveredPeer>>>,
        shutdown_flag: Arc<RwLock<bool>>,
        service_type: &str,
    ) {
        loop {
            if *shutdown_flag.read() {
                break;
            }

            match receiver.recv_timeout(std::time::Duration::from_secs(1)) {
                Ok(event) => match event {
                    ServiceEvent::ServiceResolved(info) => {
                        // Extract peer info from resolved service
                        let node_id = info
                            .get_property_val_str("node_id")
                            .map(|s| s.to_string())
                            .unwrap_or_else(|| info.get_fullname().to_string());

                        let public_key = info.get_property_val_str("pubkey").map(|s| s.to_string());

                        // Get address
                        let addrs: Vec<_> = info.get_addresses().iter().collect();
                        if let Some(addr) = addrs.first() {
                            let cluster_addr = format!("{}:{}", addr, info.get_port());

                            let peer = DiscoveredPeer {
                                node_id: node_id.clone(),
                                cluster_addr: cluster_addr.clone(),
                                public_key,
                            };

                            tracing::info!(
                                node_id = %node_id,
                                addr = %cluster_addr,
                                "Discovered peer via mDNS"
                            );

                            peers.write().insert(peer);
                        }
                    }
                    ServiceEvent::ServiceRemoved(_, fullname) => {
                        // Remove the peer whose service departed. Match the
                        // resolved instance label exactly rather than testing
                        // whether a peer's node_id appears anywhere in the full
                        // name — a substring test evicts unrelated peers (see
                        // peer_matches_removed_service).
                        peers.write().retain(|p| {
                            !peer_matches_removed_service(&p.node_id, &fullname, service_type)
                        });
                        tracing::debug!(fullname = %fullname, "mDNS service removed");
                    }
                    _ => {}
                },
                Err(e) => {
                    // Timeout - check shutdown flag and continue
                    // Disconnected - break
                    let err_str = format!("{e:?}");
                    if err_str.contains("Disconnected") {
                        break;
                    }
                    // Timeout - continue
                    continue;
                }
            }
        }
    }

    /// Shutdown mDNS discovery
    #[allow(dead_code)] // Reserved for graceful shutdown API
    pub fn shutdown(self) {
        *self.shutdown_flag.write() = true;
        // Daemon is dropped, which unregisters the service
    }
}

/// Decide whether a peer corresponds to the mDNS service reported by a
/// `ServiceRemoved` event.
///
/// `ServiceRemoved` carries the departing service's full name, which has the
/// form `<node_id>.<service_type>` (e.g. `node1._llama-mesh._tcp.local.`). We
/// recover the instance label (the `node_id`) by stripping the service-type
/// suffix and compare it to the peer's `node_id` exactly. A full-name match is
/// also accepted, to cover peers discovered without a `node_id` TXT record —
/// those fall back to the full name as their id (see the `ServiceResolved`
/// arm).
///
/// Matching exactly — rather than testing `fullname.contains(node_id)` — avoids
/// evicting an unrelated, still-alive peer whose `node_id` merely appears as a
/// substring of the departing service's full name: e.g. `node` when `node1`
/// departs, or a node named `tcp`/`local` whose id is part of the service type
/// present in every full name.
fn peer_matches_removed_service(node_id: &str, fullname: &str, service_type: &str) -> bool {
    let node_id = node_id.trim_end_matches('.');
    let fullname = fullname.trim_end_matches('.');
    let service_type = service_type.trim_end_matches('.');

    let instance = fullname
        .strip_suffix(service_type)
        .map_or(fullname, |label| label.trim_end_matches('.'));

    !node_id.is_empty() && (node_id == instance || node_id == fullname)
}

#[cfg(test)]
mod tests {
    use super::peer_matches_removed_service;
    use crate::discovery::DiscoveredPeer;
    use std::collections::HashSet;

    const SERVICE_TYPE: &str = "_llama-mesh._tcp.local.";
    const FULLNAME: &str = "node1._llama-mesh._tcp.local.";

    fn peer(node_id: &str) -> DiscoveredPeer {
        DiscoveredPeer {
            node_id: node_id.to_string(),
            cluster_addr: format!("{node_id}:8080"),
            public_key: None,
        }
    }

    #[test]
    fn matches_exact_departing_node() {
        assert!(peer_matches_removed_service(
            "node1",
            FULLNAME,
            SERVICE_TYPE
        ));
    }

    #[test]
    fn does_not_match_node_id_substring_of_departing_node() {
        // "node" is a substring of the departing "node1"; the old
        // `fullname.contains(node_id)` check wrongly evicted it.
        assert!(!peer_matches_removed_service(
            "node",
            FULLNAME,
            SERVICE_TYPE
        ));
    }

    #[test]
    fn does_not_match_node_id_substring_of_service_type() {
        // A node whose id appears inside the service type would be evicted on
        // *every* departure under the old substring check.
        for victim in ["tcp", "local", "llama", "mesh", "_tcp"] {
            assert!(
                !peer_matches_removed_service(victim, FULLNAME, SERVICE_TYPE),
                "{victim} must not match an unrelated departure"
            );
        }
    }

    #[test]
    fn matches_fullname_fallback_id() {
        // Peers discovered without a node_id TXT record fall back to using the
        // full name as their id; a removal must still match them.
        assert!(peer_matches_removed_service(
            FULLNAME,
            FULLNAME,
            SERVICE_TYPE
        ));
    }

    #[test]
    fn tolerates_missing_trailing_dot() {
        assert!(peer_matches_removed_service(
            "node1",
            "node1._llama-mesh._tcp.local",
            "_llama-mesh._tcp.local"
        ));
    }

    #[test]
    fn empty_node_id_never_matches() {
        assert!(!peer_matches_removed_service("", FULLNAME, SERVICE_TYPE));
    }

    #[test]
    fn retain_evicts_only_the_departing_peer() {
        // End-to-end check of the `ServiceRemoved` retain over a peer set with
        // substring-colliding ids. Under the old substring check, "node1",
        // "node", and "tcp" would all be removed; only "node1" should be.
        let mut peers: HashSet<DiscoveredPeer> = HashSet::new();
        peers.insert(peer("node1")); // the departing peer
        peers.insert(peer("node")); // substring of "node1"
        peers.insert(peer("tcp")); // substring of the service type
        peers.insert(peer("other")); // unrelated

        peers.retain(|p| !peer_matches_removed_service(&p.node_id, FULLNAME, SERVICE_TYPE));

        let remaining: HashSet<&str> = peers.iter().map(|p| p.node_id.as_str()).collect();
        assert_eq!(
            remaining.len(),
            3,
            "only the departing peer should be removed"
        );
        assert!(remaining.contains("node"));
        assert!(remaining.contains("tcp"));
        assert!(remaining.contains("other"));
        assert!(!remaining.contains("node1"));
    }
}
