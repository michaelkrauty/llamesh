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
    #[cfg(test)]
    check_os_ports: bool,
}

impl PortPool {
    /// Create a new port pool with optional port configuration.
    pub fn new(ports_config: Option<LlamaCppPorts>) -> Self {
        Self {
            used_ports: Arc::new(RwLock::new(HashSet::new())),
            ports_config,
            #[cfg(test)]
            check_os_ports: true,
        }
    }

    #[cfg(test)]
    fn new_without_os_port_check(ports_config: Option<LlamaCppPorts>) -> Self {
        Self {
            used_ports: Arc::new(RwLock::new(HashSet::new())),
            ports_config,
            check_os_ports: false,
        }
    }

    /// Get an available port from the pool.
    pub async fn get_available_port(&self) -> Result<u16, PortError> {
        let mut used = self.used_ports.write().await;

        if let Some(ports_config) = &self.ports_config {
            // Check explicit ports first
            if let Some(explicit_ports) = &ports_config.ports {
                for port in explicit_ports {
                    if !used.contains(port) && self.is_port_available(*port).await {
                        used.insert(*port);
                        return Ok(*port);
                    }
                }
            }
            // Check port ranges
            if let Some(ranges) = &ports_config.ranges {
                for range in ranges {
                    for port in range.start..=range.end {
                        if !used.contains(&port) && self.is_port_available(port).await {
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
                if !used.contains(&port) && self.is_port_available(port).await {
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
    async fn is_port_available(&self, port: u16) -> bool {
        #[cfg(test)]
        if !self.check_os_ports {
            return true;
        }

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
    use std::sync::atomic::{AtomicUsize, Ordering};

    static NEXT_TEST_PORT: AtomicUsize = AtomicUsize::new(20_000);

    async fn free_test_port() -> u16 {
        free_test_ports(1).await.remove(0)
    }

    async fn free_test_ports(count: usize) -> Vec<u16> {
        loop {
            let start = NEXT_TEST_PORT.fetch_add(count + 1, Ordering::SeqCst);
            if start + count >= 32_000 {
                panic!("exhausted low test port range");
            }

            let ports: Vec<u16> = (start..start + count).map(|port| port as u16).collect();
            let mut listeners = Vec::with_capacity(count);

            for port in &ports {
                match tokio::net::TcpListener::bind(("127.0.0.1", *port)).await {
                    Ok(listener) => listeners.push(listener),
                    Err(_) => {
                        listeners.clear();
                        break;
                    }
                }
            }

            if listeners.len() == count {
                drop(listeners);
                return ports;
            }
        }
    }

    #[tokio::test]
    async fn test_get_available_port_ephemeral() {
        let pool = PortPool::new(None);
        let port = pool.get_available_port().await.unwrap();
        assert!((10000..20000).contains(&port));
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
        let port = free_test_port().await;
        pool.reserve_specific_port(port).await.unwrap();
        assert!(pool.is_reserved(port).await);

        // Second reservation should fail
        let result = pool.reserve_specific_port(port).await;
        assert!(matches!(result, Err(PortError::PortInUse(p)) if p == port));
    }

    #[tokio::test]
    async fn test_configured_port_range() {
        let port = free_test_port().await;
        let config = LlamaCppPorts {
            ports: None,
            ranges: Some(vec![PortRange {
                start: port,
                end: port,
            }]),
        };
        let pool = PortPool::new_without_os_port_check(Some(config));

        assert_eq!(pool.get_available_port().await.unwrap(), port);
    }

    #[tokio::test]
    async fn test_port_exhaustion() {
        let ports = free_test_ports(3).await;
        let config = LlamaCppPorts {
            ports: Some(ports.clone()),
            ranges: None,
        };
        let pool = PortPool::new_without_os_port_check(Some(config));

        for port in &ports {
            pool.reserve_specific_port(*port).await.unwrap();
        }

        let result = pool.get_available_port().await;
        assert!(matches!(result, Err(PortError::NoAvailablePorts)));
    }

    #[tokio::test]
    async fn test_release_nonexistent_port() {
        let pool = PortPool::new(None);
        let port = free_test_port().await;
        // Releasing a never-reserved port should not panic
        pool.release_port(port).await;
        // And it should still not be reserved
        assert!(!pool.is_reserved(port).await);
    }

    #[tokio::test]
    async fn test_single_port_range() {
        let port = free_test_port().await;
        let config = LlamaCppPorts {
            ports: None,
            ranges: Some(vec![PortRange {
                start: port,
                end: port,
            }]),
        };
        let pool = PortPool::new_without_os_port_check(Some(config));

        assert_eq!(pool.get_available_port().await.unwrap(), port);

        // Second allocation should fail
        let result = pool.get_available_port().await;
        assert!(matches!(result, Err(PortError::NoAvailablePorts)));
    }

    #[tokio::test]
    async fn test_port_reuse_after_release() {
        let port = free_test_port().await;
        let config = LlamaCppPorts {
            ports: None,
            ranges: Some(vec![PortRange {
                start: port,
                end: port,
            }]),
        };
        let pool = PortPool::new_without_os_port_check(Some(config));

        let port1 = pool.get_available_port().await.unwrap();
        pool.release_port(port1).await;
        let port2 = pool.get_available_port().await.unwrap();

        // Should get the same port back
        assert_eq!(port1, port2);
    }
}
