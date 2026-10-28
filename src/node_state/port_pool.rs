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

/// A port taken from the pool for an instance that is not yet in the instance
/// map. Dropping the lease, as happens when the spawn is cancelled, returns
/// the port; once the instance is in the map, `handoff` leaves the port to the
/// instance's cleanup instead.
pub struct PortLease {
    used_ports: Arc<RwLock<HashSet<u16>>>,
    port: u16,
    armed: bool,
}

impl PortLease {
    fn new(used_ports: Arc<RwLock<HashSet<u16>>>, port: u16) -> Self {
        Self {
            used_ports,
            port,
            armed: true,
        }
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// Return the port to the pool now, rather than possibly later from a task
    /// spawned by `Drop`, so the next allocation sees it free.
    pub async fn release(mut self) {
        self.used_ports.write().await.remove(&self.port);
        self.armed = false;
    }

    /// Hand the port to the instance now in the instance map, whose cleanup
    /// releases it.
    pub fn handoff(mut self) -> u16 {
        self.armed = false;
        self.port
    }
}

impl Drop for PortLease {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Ok(mut used) = self.used_ports.try_write() {
            used.remove(&self.port);
            return;
        }

        let used_ports = self.used_ports.clone();
        let port = self.port;
        match tokio::runtime::Handle::try_current() {
            Ok(handle) => {
                handle.spawn(async move {
                    used_ports.write().await.remove(&port);
                });
            }
            Err(_) => {
                warn!(
                    port = port,
                    "Port cleanup skipped: tokio runtime unavailable during drop"
                );
            }
        }
    }
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
    pub(super) fn new_without_os_port_check(ports_config: Option<LlamaCppPorts>) -> Self {
        Self {
            used_ports: Arc::new(RwLock::new(HashSet::new())),
            ports_config,
            check_os_ports: false,
        }
    }

    /// Get an available port from the pool.
    pub async fn get_available_port(&self) -> Result<PortLease, PortError> {
        let mut used = self.used_ports.write().await;

        if let Some(ports_config) = &self.ports_config {
            // Check explicit ports first
            if let Some(explicit_ports) = &ports_config.ports {
                for port in explicit_ports {
                    if !used.contains(port) && self.is_port_available(*port).await {
                        used.insert(*port);
                        return Ok(PortLease::new(self.used_ports.clone(), *port));
                    }
                }
            }
            // Check port ranges
            if let Some(ranges) = &ports_config.ranges {
                for range in ranges {
                    for port in range.start..=range.end {
                        if !used.contains(&port) && self.is_port_available(port).await {
                            used.insert(port);
                            return Ok(PortLease::new(self.used_ports.clone(), port));
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
                    return Ok(PortLease::new(self.used_ports.clone(), port));
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
    pub async fn reserve_specific_port(&self, port: u16) -> Result<PortLease, PortError> {
        let mut used = self.used_ports.write().await;
        if used.contains(&port) {
            return Err(PortError::PortInUse(port));
        }
        used.insert(port);
        Ok(PortLease::new(self.used_ports.clone(), port))
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

    async fn test_lease(specific: bool) -> (PortPool, PortLease) {
        let port = 24_237;
        let pool = PortPool::new_without_os_port_check(Some(LlamaCppPorts {
            ports: Some(vec![port]),
            ranges: None,
        }));
        let lease = if specific {
            pool.reserve_specific_port(port).await.unwrap()
        } else {
            pool.get_available_port().await.unwrap()
        };
        assert_eq!(lease.port(), port);
        assert!(pool.is_reserved(port).await);
        (pool, lease)
    }

    #[tokio::test]
    async fn test_dropped_port_lease_releases_port() {
        for specific in [false, true] {
            let (pool, lease) = test_lease(specific).await;
            let port = lease.port();
            drop(lease);
            assert!(!pool.is_reserved(port).await);
        }
    }

    #[tokio::test]
    async fn test_dropped_port_lease_releases_port_after_contention() {
        for specific in [false, true] {
            let (pool, lease) = test_lease(specific).await;
            let port = lease.port();
            let used = pool.used_ports.read().await;
            drop(lease);
            assert!(used.contains(&port));
            drop(used);
            tokio::time::timeout(std::time::Duration::from_secs(1), async {
                while pool.is_reserved(port).await {
                    tokio::task::yield_now().await;
                }
            })
            .await
            .expect("dropped lease must release the port after lock contention");
        }
    }

    #[tokio::test]
    async fn test_explicitly_released_port_lease_releases_port() {
        for specific in [false, true] {
            let (pool, lease) = test_lease(specific).await;
            let port = lease.port();
            lease.release().await;
            assert!(!pool.is_reserved(port).await);
            let replacement = pool.get_available_port().await.unwrap();
            assert_eq!(replacement.port(), port);
        }
    }

    #[tokio::test]
    async fn test_handed_off_port_lease_keeps_port_reserved() {
        for specific in [false, true] {
            let (pool, lease) = test_lease(specific).await;
            let port = lease.handoff();
            assert!(pool.is_reserved(port).await);
            pool.release_port(port).await;
            assert!(!pool.is_reserved(port).await);
        }
    }

    #[tokio::test]
    async fn test_get_available_port_ephemeral() {
        let pool = PortPool::new(None);
        let port = pool.get_available_port().await.unwrap().handoff();
        assert!((10000..20000).contains(&port));
        assert!(pool.is_reserved(port).await);
    }

    #[tokio::test]
    async fn test_release_port() {
        let pool = PortPool::new(None);
        let port = pool.get_available_port().await.unwrap().handoff();
        assert!(pool.is_reserved(port).await);
        pool.release_port(port).await;
        assert!(!pool.is_reserved(port).await);
    }

    #[tokio::test]
    async fn test_reserve_specific_port() {
        let pool = PortPool::new(None);
        let port = free_test_port().await;
        pool.reserve_specific_port(port).await.unwrap().handoff();
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

        assert_eq!(pool.get_available_port().await.unwrap().handoff(), port);
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
            pool.reserve_specific_port(*port).await.unwrap().handoff();
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

        assert_eq!(pool.get_available_port().await.unwrap().handoff(), port);

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

        let port1 = pool.get_available_port().await.unwrap().handoff();
        pool.release_port(port1).await;
        let port2 = pool.get_available_port().await.unwrap().handoff();

        // Should get the same port back
        assert_eq!(port1, port2);
    }
}
