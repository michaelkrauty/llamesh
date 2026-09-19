//! mDNS-based peer discovery for zero-config LAN operation.
//!
//! Announces and discovers peers on local network.
//! Service type: _llama-mesh._tcp.local
//! TXT records: node_id, pubkey

use super::DiscoveredPeer;
use mdns_sd::{IfKind, ScopedIp, ServiceDaemon, ServiceEvent, ServiceInfo};
use parking_lot::RwLock;
use std::collections::{HashMap, HashSet};
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use std::thread;

#[cfg(all(test, target_os = "linux"))]
#[path = "mdns_network_tests.rs"]
mod network_tests;

/// mDNS discovery handler
pub struct MdnsDiscovery {
    /// Explicit shutdown is required; dropping the upstream handle does not stop it.
    daemon: ServiceDaemon,
}

impl MdnsDiscovery {
    /// Create and start mDNS discovery
    pub fn new(
        service_type: &str,
        node_id: &str,
        listen_addr: SocketAddr,
        public_key: &str,
        peers: Arc<RwLock<HashSet<DiscoveredPeer>>>,
    ) -> anyhow::Result<Self> {
        // Own the daemon immediately so errors during setup also shut it down.
        let discovery = Self {
            daemon: ServiceDaemon::new()?,
        };

        // Normalize service type to end with ".local." as required by mdns-sd
        // Input might be "_llama-mesh._tcp.local" or "_llama-mesh._tcp"
        let service_type = service_type.trim_end_matches('.');
        let service_type = service_type.trim_end_matches(".local");
        let service_type = format!("{service_type}.local.");

        // DNS address records are hostname-wide. Avoid mixing this listener's
        // addresses with records from other services using the machine hostname.
        let hostname = format!(
            "llamesh-{}.local.",
            ulid::Ulid::new().to_string().to_lowercase()
        );

        // Create service info
        let properties = vec![
            ("node_id".to_string(), node_id.to_string()),
            ("pubkey".to_string(), public_key.to_string()),
        ];

        let mut service_info = ServiceInfo::new(
            &service_type,
            node_id,
            &hostname,
            (),
            listen_addr.port(),
            properties.as_slice(),
        )?
        .enable_addr_auto();
        service_info.set_interfaces(vec![match listen_addr.ip() {
            IpAddr::V4(ip) if ip.is_unspecified() => IfKind::IPv4,
            IpAddr::V6(ip) if ip.is_unspecified() => IfKind::IPv6,
            ip => IfKind::Addr(ip),
        }]);

        // Register our service
        discovery.daemon.register(service_info)?;

        tracing::info!(
            service_type = %service_type,
            node_id = %node_id,
            port = listen_addr.port(),
            "Registered mDNS service"
        );

        // Browse for peers
        let receiver = discovery.daemon.browse(&service_type)?;
        let local_node_id = node_id.to_string();

        thread::Builder::new()
            .name("mdns-peers".into())
            .spawn(move || Self::browse_loop(receiver, peers, &local_node_id))?;

        Ok(discovery)
    }

    /// Background loop to process mDNS events
    fn browse_loop(
        receiver: mdns_sd::Receiver<ServiceEvent>,
        peers: Arc<RwLock<HashSet<DiscoveredPeer>>>,
        local_node_id: &str,
    ) {
        // Per-interface conflict renames can give one node several fullnames.
        // Keep each contribution so removing one preserves the others.
        let mut service_peers = HashMap::new();
        while let Ok(event) = receiver.recv() {
            match event {
                ServiceEvent::ServiceResolved(info) => {
                    // Extract peer info from resolved service
                    let node_id = info
                        .get_property_val_str("node_id")
                        .map(|s| s.to_string())
                        .unwrap_or_else(|| info.get_fullname().to_string());

                    let public_key = info.get_property_val_str("pubkey").map(|s| s.to_string());

                    let peer = peer_endpoint(info.get_addresses(), info.get_port())
                        .filter(|_| node_id != local_node_id)
                        .map(|addr| DiscoveredPeer {
                            node_id: node_id.clone(),
                            cluster_addr: addr.to_string(),
                            public_key,
                        });
                    if let Some(peer) = &peer {
                        tracing::info!(
                            node_id = %node_id,
                            addr = %peer.cluster_addr,
                            "Discovered peer via mDNS"
                        );
                    }
                    replace_service_peer(
                        &mut service_peers,
                        &mut peers.write(),
                        info.get_fullname(),
                        peer,
                    );
                }
                ServiceEvent::ServiceRemoved(_, fullname) => {
                    replace_service_peer(&mut service_peers, &mut peers.write(), &fullname, None);
                    tracing::debug!(fullname = %fullname, "mDNS service removed");
                }
                _ => {}
            }
        }
    }

    /// Shutdown mDNS discovery
    pub fn shutdown(&self) {
        // The acknowledgement follows goodbye packets and closes the browse
        // channel. Bound the wait in case the upstream worker has failed.
        if let Ok(done) = self.daemon.shutdown() {
            let _ = done.recv_timeout(std::time::Duration::from_secs(1));
        }
    }
}

impl Drop for MdnsDiscovery {
    fn drop(&mut self) {
        self.shutdown();
    }
}

fn peer_endpoint(addresses: &HashSet<ScopedIp>, port: u16) -> Option<SocketAddr> {
    addresses
        .iter()
        .map(ScopedIp::to_ip_addr)
        // URL-based cluster transports cannot represent an IPv6 zone ID. Do not
        // silently discard the scope and select an unreachable link-local IP.
        .filter(|ip| match ip {
            IpAddr::V4(ip) => !ip.is_unspecified() && !ip.is_multicast() && !ip.is_loopback(),
            IpAddr::V6(ip) => {
                !ip.is_unspecified()
                    && !ip.is_multicast()
                    && !ip.is_loopback()
                    && !ip.is_unicast_link_local()
            }
        })
        .min() // IpAddr orders IPv4 before IPv6, deterministically within each family.
        .map(|ip| SocketAddr::new(ip, port))
}

/// Reconcile only nodes contributed by this service. Unknown removals must not
/// infer ownership from instance labels or affect explicitly configured peers.
fn replace_service_peer(
    services: &mut HashMap<String, DiscoveredPeer>,
    peers: &mut HashSet<DiscoveredPeer>,
    fullname: &str,
    peer: Option<DiscoveredPeer>,
) {
    let fullname = fullname.trim_end_matches('.').to_ascii_lowercase();
    let previous = match &peer {
        Some(peer) => services.insert(fullname, peer.clone()),
        None => services.remove(&fullname),
    };
    for affected in previous.iter().chain(peer.iter()) {
        peers.retain(|p| p.node_id != affected.node_id);
        // Stable across event order, while keeping one endpoint per node.
        if let Some((_, selected)) = services
            .iter()
            .filter(|(_, p)| p.node_id == affected.node_id)
            .min_by_key(|(name, _)| *name)
        {
            peers.insert(selected.clone());
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{peer_endpoint, replace_service_peer};
    use crate::discovery::DiscoveredPeer;
    use mdns_sd::ScopedIp;
    use std::collections::{HashMap, HashSet};
    use std::net::IpAddr;

    const SERVICE_TYPE: &str = "_llama-mesh._tcp.local.";
    const FULLNAME: &str = "node1._llama-mesh._tcp.local.";

    #[test]
    fn endpoint_selection_is_stable_and_url_compatible() {
        let addresses = |ips: &[&str]| -> HashSet<ScopedIp> {
            ips.iter()
                .map(|ip| ip.parse::<IpAddr>().unwrap().into())
                .collect()
        };
        for (ips, expected) in [
            (
                vec!["fd00::1", "192.0.2.2", "192.0.2.1"],
                Some("192.0.2.1:8080"),
            ),
            (
                vec!["fe80::1", "fd00::2", "fd00::1"],
                Some("[fd00::1]:8080"),
            ),
            (
                vec![
                    "fe80::1",
                    "127.0.0.1",
                    "::1",
                    "0.0.0.0",
                    "::",
                    "ff02::1",
                    "224.0.0.251",
                ],
                None,
            ),
        ] {
            let endpoint = peer_endpoint(&addresses(&ips), 8080).map(|addr| addr.to_string());
            assert_eq!(endpoint.as_deref(), expected);
            if let Some(endpoint) = endpoint {
                let url = reqwest::Url::parse(&format!("http://{endpoint}")).unwrap();
                assert_eq!(url.port(), Some(8080));
            }
        }
    }

    fn peer(node_id: &str) -> DiscoveredPeer {
        DiscoveredPeer {
            node_id: node_id.to_string(),
            cluster_addr: format!("{node_id}:8080"),
            public_key: None,
        }
    }

    #[test]
    fn removing_either_service_preserves_the_same_nodes_other_endpoint() {
        let mut second = peer("node1");
        second.cluster_addr = "192.0.2.2:9000".into();
        let contributions = [
            (FULLNAME.to_string(), peer("node1")),
            (format!("renamed.{SERVICE_TYPE}"), second),
        ];

        for reverse_order in [false, true] {
            for removed in 0..2 {
                let mut services = HashMap::new();
                let mut peers = HashSet::new();
                for index in if reverse_order { [1, 0] } else { [0, 1] } {
                    let (name, peer) = &contributions[index];
                    replace_service_peer(&mut services, &mut peers, name, Some(peer.clone()));
                }
                assert_eq!(peers, HashSet::from([contributions[0].1.clone()]));
                replace_service_peer(&mut services, &mut peers, &contributions[removed].0, None);
                assert_eq!(peers, HashSet::from([contributions[1 - removed].1.clone()]));
                replace_service_peer(
                    &mut services,
                    &mut peers,
                    &contributions[1 - removed].0,
                    None,
                );
                assert!(peers.is_empty());
            }
        }
    }

    #[test]
    fn service_updates_reconcile_old_and_new_txt_identities() {
        let mut services = HashMap::new();
        let mut peers = HashSet::new();
        let alias = format!("renamed.{SERVICE_TYPE}");
        replace_service_peer(&mut services, &mut peers, FULLNAME, Some(peer("node1")));
        replace_service_peer(&mut services, &mut peers, &alias, Some(peer("node1")));
        replace_service_peer(&mut services, &mut peers, FULLNAME, Some(peer("node2")));
        assert_eq!(peers, HashSet::from([peer("node1"), peer("node2")]));
        replace_service_peer(&mut services, &mut peers, &alias, None);
        assert_eq!(peers, HashSet::from([peer("node2")]));
    }

    #[test]
    fn removals_preserve_unrelated_and_explicit_peers() {
        let mut services = HashMap::new();
        let unaffected: HashSet<_> = [
            "node",
            "tcp",
            "local",
            "llama",
            "mesh",
            "_tcp",
            "other",
            "explicit:192.0.2.1:8080",
        ]
        .into_iter()
        .map(peer)
        .collect();
        let mut peers = unaffected.clone();
        replace_service_peer(&mut services, &mut peers, FULLNAME, Some(peer("node1")));
        // DNS name case and the trailing root dot do not change ownership.
        replace_service_peer(
            &mut services,
            &mut peers,
            "NODE1._llama-mesh._tcp.local",
            None,
        );
        assert_eq!(peers, unaffected);
        for name in ["other", "explicit:192.0.2.1:8080"] {
            replace_service_peer(
                &mut services,
                &mut peers,
                &format!("{name}.{SERVICE_TYPE}"),
                None,
            );
        }
        assert_eq!(peers, unaffected, "unknown services own no peers");
    }

    #[test]
    fn removal_supports_fullname_fallback_identity() {
        let mut services = HashMap::new();
        let mut peers = HashSet::new();
        replace_service_peer(&mut services, &mut peers, FULLNAME, Some(peer(FULLNAME)));
        assert_eq!(peers, HashSet::from([peer(FULLNAME)]));
        replace_service_peer(&mut services, &mut peers, FULLNAME, None);
        assert!(peers.is_empty());
    }
}
