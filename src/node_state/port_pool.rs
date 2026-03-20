//! Port allocation pool for llama-server instances.

use crate::config::LlamaCppPorts;
use std::collections::HashSet;
use std::sync::Arc;
use tokio::sync::RwLock;
use tracing::warn;

/// Error when no ports are available.
#[derive(Debug, thiserror::Error)]
pub enum PortError {
    #[error("No available ports")]
    NoAvailablePorts,
    #[error("Port {0} already in use")]
    PortInUse(u16),
}

/// Manages port allocation for llama-server instances.
pub struct PortPool {
    used_ports: Arc<RwLock<HashSet<u16>>>,
    ports_config: Option<LlamaCppPorts>,
}

impl PortPool {
    /// Create a new port pool with optional port configuration.
    pub fn new(ports_config: Option<LlamaCppPorts>) -> Self {
        Self {
            used_ports: Arc::new(RwLock::new(HashSet::new())),
            ports_config,
        }
    }

    /// Get an available port from the pool.
    pub async fn get_available_port(&self) -> Result<u16, PortError> {
        let mut used = self.used_ports.write().await;

        if let Some(ports_config) = &self.ports_config {
            // Check explicit ports first
            if let Some(explicit_ports) = &ports_config.ports {
                for port in explicit_ports {
                    if !used.contains(port) && Self::is_port_available(*port).await {
                        used.insert(*port);
                        return Ok(*port);
                    }
                }
            }
            // Check port ranges
            if let Some(ranges) = &ports_config.ranges {
                for range in ranges {
                    for port in range.start..=range.end {
                        if !used.contains(&port) && Self::is_port_available(port).await {
                            used.insert(port);
                            return Ok(port);
                        }
                    }
                }
            }
            warn!("No available ports found in configured ranges/lists");
        } else {
            // Use ephemeral port range with random start
            use rand::Rng;
            let start = {
                let mut rng = rand::rng();
                rng.random_range(10000..20000)
            };

            for port in (start..20000).chain(10000..start) {
                if !used.contains(&port) && Self::is_port_available(port).await {
                    used.insert(port);
                    return Ok(port);
                }
            }
            warn!("No available ephemeral ports found");
        }

        Err(PortError::NoAvailablePorts)
    }

    /// Check if a port is available on the system.
    ///
    /// Note: There's an inherent TOCTOU race here - a port could be taken between
    /// this check and llama-server's bind. This is acceptable because:
    /// 1. The instance spawn will fail with "address in use"
    /// 2. try_get_or_spawn() retries with a different port
    /// 3. The port pool correctly releases failed allocations
    async fn is_port_available(port: u16) -> bool {
        tokio::net::TcpListener::bind(("127.0.0.1", port))
            .await
            .is_ok()
    }

    /// Release a port back to the pool.
    pub async fn release_port(&self, port: u16) {
        let mut used = self.used_ports.write().await;
        used.remove(&port);
    }

    /// Reserve a specific port.
    pub async fn reserve_specific_port(&self, port: u16) -> Result<(), PortError> {
        let mut used = self.used_ports.write().await;
        if used.contains(&port) {
            return Err(PortError::PortInUse(port));
        }
        used.insert(port);
        Ok(())
    }

    /// Check if a port is currently reserved (for testing).
    #[cfg(test)]
    pub async fn is_reserved(&self, port: u16) -> bool {
        let used = self.used_ports.read().await;
        used.contains(&port)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::PortRange;

    #[tokio::test]
    async fn test_get_available_port_ephemeral() {
        let pool = PortPool::new(None);
        let port = pool.get_available_port().await.unwrap();
        assert!(port >= 10000 && port < 20000);
        assert!(pool.is_reserved(port).await);
    }

    #[tokio::test]
    async fn test_release_port() {
        let pool = PortPool::new(None);
        let port = pool.get_available_port().await.unwrap();
        assert!(pool.is_reserved(port).await);
        pool.release_port(port).await;
        assert!(!pool.is_reserved(port).await);
    }

    #[tokio::test]
    async fn test_reserve_specific_port() {
        let pool = PortPool::new(None);
        pool.reserve_specific_port(30000).await.unwrap();
        assert!(pool.is_reserved(30000).await);

        // Second reservation should fail
        let result = pool.reserve_specific_port(30000).await;
        assert!(matches!(result, Err(PortError::PortInUse(30000))));
    }

    #[tokio::test]
    async fn test_configured_port_range() {
        let config = LlamaCppPorts {
            ports: None,
            ranges: Some(vec![PortRange {
                start: 31000,
                end: 31005,
            }]),
        };
        let pool = PortPool::new(Some(config));

        let port = pool.get_available_port().await.unwrap();
        assert!(port >= 31000 && port <= 31005);
    }

    #[tokio::test]
    async fn test_port_exhaustion() {
        let config = LlamaCppPorts {
            ports: None,
            ranges: Some(vec![PortRange {
                start: 32000,
                end: 32002, // Only 3 ports
            }]),
        };
        let pool = PortPool::new(Some(config));

        // Reserve all available ports
        let p1 = pool.get_available_port().await.unwrap();
        let p2 = pool.get_available_port().await.unwrap();
        let p3 = pool.get_available_port().await.unwrap();

        // All ports should be different
        assert_ne!(p1, p2);
        assert_ne!(p2, p3);
        assert_ne!(p1, p3);

        // Next allocation should fail
        let result = pool.get_available_port().await;
        assert!(matches!(result, Err(PortError::NoAvailablePorts)));
    }

    #[tokio::test]
    async fn test_release_nonexistent_port() {
        let pool = PortPool::new(None);
        // Releasing a never-reserved port should not panic
        pool.release_port(60000).await;
        // And it should still not be reserved
        assert!(!pool.is_reserved(60000).await);
    }

    #[tokio::test]
    async fn test_single_port_range() {
        let config = LlamaCppPorts {
            ports: None,
            ranges: Some(vec![PortRange {
                start: 33000,
                end: 33000, // Single port
            }]),
        };
        let pool = PortPool::new(Some(config));

        let port = pool.get_available_port().await.unwrap();
        assert_eq!(port, 33000);

        // Second allocation should fail
        let result = pool.get_available_port().await;
        assert!(matches!(result, Err(PortError::NoAvailablePorts)));
    }

    #[tokio::test]
    async fn test_port_reuse_after_release() {
        let config = LlamaCppPorts {
            ports: None,
            ranges: Some(vec![PortRange {
                start: 36000,
                end: 36000, // Single port
            }]),
        };
        let pool = PortPool::new(Some(config));

        let port1 = pool.get_available_port().await.unwrap();
        pool.release_port(port1).await;
        let port2 = pool.get_available_port().await.unwrap();

        // Should get the same port back
        assert_eq!(port1, port2);
    }
}
