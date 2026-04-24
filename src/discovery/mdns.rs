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
        _service_type: &str,
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
                        // Remove peer when service goes away
                        peers.write().retain(|p| !fullname.contains(&p.node_id));
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
