//! Circuit breaker for peer connections.
//!
//! Prevents cascading failures by tracking peer health and temporarily
//! blocking requests to unhealthy peers.
//!
//! State transitions:
//! - Closed → Open: failure_count >= threshold
//! - Open → HalfOpen: backoff elapsed
//! - HalfOpen → Closed: success_count >= threshold
//! - HalfOpen → Open: any failure (exponential backoff increase)

use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::RwLock;

/// Circuit breaker states
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CircuitState {
    /// Normal operation - requests allowed
    Closed,
    /// Circuit is open - requests blocked
    Open,
    /// Testing recovery - limited requests allowed
    HalfOpen,
}

/// Per-peer circuit state
#[derive(Debug)]
pub struct PeerCircuit {
    pub state: CircuitState,
    pub failure_count: u32,
    pub success_count: u32,
    pub last_failure: Option<Instant>,
    pub last_state_change: Instant,
    pub consecutive_opens: u32,
}

impl Default for PeerCircuit {
    fn default() -> Self {
        Self {
            state: CircuitState::Closed,
            failure_count: 0,
            success_count: 0,
            last_failure: None,
            last_state_change: Instant::now(),
            consecutive_opens: 0,
        }
    }
}

/// Configuration for circuit breaker behavior
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CircuitBreakerConfig {
    /// Failures before opening circuit (default: 5)
    #[serde(default = "default_failure_threshold")]
    pub failure_threshold: u32,

    /// Successes in half-open to close circuit (default: 2)
    #[serde(default = "default_success_threshold")]
    pub success_threshold: u32,

    /// Base backoff duration in ms (default: 5000)
    #[serde(default = "default_open_duration_base_ms")]
    pub open_duration_base_ms: u64,

    /// Max backoff duration in ms (default: 60000)
    #[serde(default = "default_open_duration_max_ms")]
    pub open_duration_max_ms: u64,

    /// Enable circuit breaker (default: true)
    #[serde(default = "default_enabled")]
    pub enabled: bool,
}

fn default_failure_threshold() -> u32 {
    5
}
fn default_success_threshold() -> u32 {
    2
}
fn default_open_duration_base_ms() -> u64 {
    5000
}
fn default_open_duration_max_ms() -> u64 {
    60000
}
fn default_enabled() -> bool {
    true
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: default_failure_threshold(),
            success_threshold: default_success_threshold(),
            open_duration_base_ms: default_open_duration_base_ms(),
            open_duration_max_ms: default_open_duration_max_ms(),
            enabled: default_enabled(),
        }
    }
}

/// Circuit breaker state manager
pub struct CircuitBreaker {
    circuits: Arc<RwLock<HashMap<String, PeerCircuit>>>,
    config: CircuitBreakerConfig,
}

#[allow(dead_code)]
impl CircuitBreaker {
    /// Create a new circuit breaker with the given config
    pub fn new(config: CircuitBreakerConfig) -> Self {
        Self {
            circuits: Arc::new(RwLock::new(HashMap::new())),
            config,
        }
    }

    /// Check if a request to the given peer should be allowed
    pub async fn should_allow(&self, peer_id: &str) -> bool {
        if !self.config.enabled {
            return true;
        }

        let mut circuits = self.circuits.write().await;
        let circuit = circuits.entry(peer_id.to_string()).or_default();

        match circuit.state {
            CircuitState::Closed => true,
            CircuitState::Open => {
                // Check if backoff has elapsed
                let backoff = self.calculate_backoff(circuit.consecutive_opens);
                if circuit.last_state_change.elapsed() >= backoff {
                    // Transition to half-open
                    circuit.state = CircuitState::HalfOpen;
                    circuit.last_state_change = Instant::now();
                    circuit.success_count = 0;
                    tracing::info!(
                        peer_id = %peer_id,
                        "Circuit breaker transitioning to half-open"
                    );
                    true
                } else {
                    false
                }
            }
            CircuitState::HalfOpen => {
                // Allow limited probes in half-open
                true
            }
        }
    }

    /// Check if a request should be allowed (synchronous, read-only check)
    ///
    /// Unlike `should_allow`, this doesn't transition states - it just checks current state.
    /// Returns true if circuit is closed/half-open, false if open and backoff hasn't elapsed.
    /// If lock can't be acquired, conservatively allows the request.
    ///
    /// Trade-off: In rare cases during high contention, a request may be allowed when the
    /// circuit should transition to half-open (Open -> HalfOpen happens in async path only).
    /// This is intentional: we prefer allowing requests over blocking, and avoid write lock
    /// contention in hot routing paths. The async `should_allow()` handles state transitions.
    pub fn should_allow_sync(&self, peer_id: &str) -> bool {
        if !self.config.enabled {
            return true;
        }

        // Try to acquire read lock without blocking
        let circuits = match self.circuits.try_read() {
            Ok(guard) => guard,
            Err(_) => return true, // Conservatively allow if lock is contended
        };

        let Some(circuit) = circuits.get(peer_id) else {
            return true; // No circuit = closed (default)
        };

        match circuit.state {
            CircuitState::Closed => true,
            CircuitState::HalfOpen => true,
            CircuitState::Open => {
                // Check if backoff has elapsed (state transition happens in async should_allow)
                let backoff = self.calculate_backoff(circuit.consecutive_opens);
                circuit.last_state_change.elapsed() >= backoff
            }
        }
    }

    /// Record a successful request to a peer
    pub async fn record_success(&self, peer_id: &str) {
        if !self.config.enabled {
            return;
        }

        let mut circuits = self.circuits.write().await;
        let circuit = circuits.entry(peer_id.to_string()).or_default();

        match circuit.state {
            CircuitState::HalfOpen => {
                circuit.success_count += 1;
                if circuit.success_count >= self.config.success_threshold {
                    // Transition to closed
                    circuit.state = CircuitState::Closed;
                    circuit.last_state_change = Instant::now();
                    circuit.failure_count = 0;
                    circuit.consecutive_opens = 0;
                    tracing::info!(
                        peer_id = %peer_id,
                        "Circuit breaker closed after successful recovery"
                    );
                }
            }
            CircuitState::Closed => {
                // Reset failure count on success
                circuit.failure_count = 0;
            }
            CircuitState::Open => {
                // Shouldn't happen - requests blocked in open state
            }
        }
    }

    /// Record a failed request to a peer
    pub async fn record_failure(&self, peer_id: &str) {
        if !self.config.enabled {
            return;
        }

        let mut circuits = self.circuits.write().await;
        let circuit = circuits.entry(peer_id.to_string()).or_default();

        circuit.failure_count += 1;
        circuit.last_failure = Some(Instant::now());

        match circuit.state {
            CircuitState::Closed => {
                if circuit.failure_count >= self.config.failure_threshold {
                    // Transition to open
                    circuit.state = CircuitState::Open;
                    circuit.last_state_change = Instant::now();
                    circuit.consecutive_opens += 1;
                    tracing::warn!(
                        peer_id = %peer_id,
                        failures = circuit.failure_count,
                        "Circuit breaker opened after consecutive failures"
                    );
                }
            }
            CircuitState::HalfOpen => {
                // Any failure in half-open goes back to open
                circuit.state = CircuitState::Open;
                circuit.last_state_change = Instant::now();
                circuit.consecutive_opens += 1;
                tracing::warn!(
                    peer_id = %peer_id,
                    "Circuit breaker reopened after failure in half-open state"
                );
            }
            CircuitState::Open => {
                // Already open
            }
        }
    }

    /// Get the current state of a peer's circuit
    pub async fn get_state(&self, peer_id: &str) -> CircuitState {
        let circuits = self.circuits.read().await;
        circuits
            .get(peer_id)
            .map(|c| c.state)
            .unwrap_or(CircuitState::Closed)
    }

    /// Get a snapshot of all circuit states for monitoring
    pub async fn get_all_states(&self) -> HashMap<String, CircuitState> {
        let circuits = self.circuits.read().await;
        circuits.iter().map(|(k, v)| (k.clone(), v.state)).collect()
    }

    /// Calculate backoff duration based on consecutive opens
    fn calculate_backoff(&self, consecutive_opens: u32) -> Duration {
        let base = self.config.open_duration_base_ms;
        let max = self.config.open_duration_max_ms;

        // Exponential backoff: base * 2^(consecutive_opens - 1), capped at max
        let exponent = consecutive_opens.saturating_sub(1).min(10);
        let backoff_ms = (base * 2u64.pow(exponent)).min(max);

        Duration::from_millis(backoff_ms)
    }

    /// Reset a peer's circuit (e.g., manual intervention)
    pub async fn reset(&self, peer_id: &str) {
        let mut circuits = self.circuits.write().await;
        circuits.remove(peer_id);
        tracing::info!(peer_id = %peer_id, "Circuit breaker reset manually");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_circuit_starts_closed() {
        let cb = CircuitBreaker::new(CircuitBreakerConfig::default());
        assert!(cb.should_allow("peer-1").await);
        assert_eq!(cb.get_state("peer-1").await, CircuitState::Closed);
    }

    #[tokio::test]
    async fn test_circuit_opens_after_failures() {
        let config = CircuitBreakerConfig {
            failure_threshold: 3,
            ..Default::default()
        };
        let cb = CircuitBreaker::new(config);

        cb.record_failure("peer-1").await;
        cb.record_failure("peer-1").await;
        assert!(cb.should_allow("peer-1").await); // Still closed

        cb.record_failure("peer-1").await;
        assert_eq!(cb.get_state("peer-1").await, CircuitState::Open);
        assert!(!cb.should_allow("peer-1").await); // Now blocked
    }

    #[tokio::test]
    async fn test_success_resets_failure_count() {
        let config = CircuitBreakerConfig {
            failure_threshold: 3,
            ..Default::default()
        };
        let cb = CircuitBreaker::new(config);

        cb.record_failure("peer-1").await;
        cb.record_failure("peer-1").await;
        cb.record_success("peer-1").await; // Reset

        cb.record_failure("peer-1").await;
        cb.record_failure("peer-1").await;
        assert!(cb.should_allow("peer-1").await); // Still closed (only 2 failures)
    }

    #[tokio::test]
    async fn test_disabled_circuit_breaker() {
        let config = CircuitBreakerConfig {
            enabled: false,
            failure_threshold: 1,
            ..Default::default()
        };
        let cb = CircuitBreaker::new(config);

        cb.record_failure("peer-1").await;
        cb.record_failure("peer-1").await;
        cb.record_failure("peer-1").await;

        // Should still allow even after many failures
        assert!(cb.should_allow("peer-1").await);
    }

    #[test]
    fn test_backoff_calculation() {
        let cb = CircuitBreaker::new(CircuitBreakerConfig {
            open_duration_base_ms: 1000,
            open_duration_max_ms: 30000,
            ..Default::default()
        });

        assert_eq!(cb.calculate_backoff(1), Duration::from_millis(1000));
        assert_eq!(cb.calculate_backoff(2), Duration::from_millis(2000));
        assert_eq!(cb.calculate_backoff(3), Duration::from_millis(4000));
        assert_eq!(cb.calculate_backoff(4), Duration::from_millis(8000));
        assert_eq!(cb.calculate_backoff(5), Duration::from_millis(16000));
        assert_eq!(cb.calculate_backoff(6), Duration::from_millis(30000)); // Capped
    }

    #[tokio::test]
    async fn test_half_open_to_closed_on_success() {
        let config = CircuitBreakerConfig {
            failure_threshold: 2,
            success_threshold: 2,
            open_duration_base_ms: 1, // 1ms for fast test
            ..Default::default()
        };
        let cb = CircuitBreaker::new(config);

        // Open the circuit
        cb.record_failure("peer-1").await;
        cb.record_failure("peer-1").await;
        assert_eq!(cb.get_state("peer-1").await, CircuitState::Open);

        // Wait for backoff and trigger half-open
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(cb.should_allow("peer-1").await); // Transitions to HalfOpen
        assert_eq!(cb.get_state("peer-1").await, CircuitState::HalfOpen);

        // Successes in half-open close the circuit
        cb.record_success("peer-1").await;
        assert_eq!(cb.get_state("peer-1").await, CircuitState::HalfOpen);
        cb.record_success("peer-1").await;
        assert_eq!(cb.get_state("peer-1").await, CircuitState::Closed);
    }

    #[tokio::test]
    async fn test_half_open_to_open_on_failure() {
        let config = CircuitBreakerConfig {
            failure_threshold: 2,
            success_threshold: 2,
            open_duration_base_ms: 1,
            ..Default::default()
        };
        let cb = CircuitBreaker::new(config);

        // Open the circuit
        cb.record_failure("peer-1").await;
        cb.record_failure("peer-1").await;
        assert_eq!(cb.get_state("peer-1").await, CircuitState::Open);

        // Wait and transition to half-open
        tokio::time::sleep(Duration::from_millis(5)).await;
        cb.should_allow("peer-1").await;
        assert_eq!(cb.get_state("peer-1").await, CircuitState::HalfOpen);

        // Failure in half-open reopens circuit
        cb.record_failure("peer-1").await;
        assert_eq!(cb.get_state("peer-1").await, CircuitState::Open);
    }

    #[tokio::test]
    async fn test_reset_clears_circuit() {
        let config = CircuitBreakerConfig {
            failure_threshold: 2,
            ..Default::default()
        };
        let cb = CircuitBreaker::new(config);

        // Open the circuit
        cb.record_failure("peer-1").await;
        cb.record_failure("peer-1").await;
        assert_eq!(cb.get_state("peer-1").await, CircuitState::Open);

        // Reset should clear state
        cb.reset("peer-1").await;
        assert_eq!(cb.get_state("peer-1").await, CircuitState::Closed);
        assert!(cb.should_allow("peer-1").await);
    }

    #[tokio::test]
    async fn test_get_all_states() {
        let config = CircuitBreakerConfig {
            failure_threshold: 1,
            ..Default::default()
        };
        let cb = CircuitBreaker::new(config);

        // Create circuits for multiple peers
        cb.should_allow("peer-1").await; // Creates closed circuit
        cb.record_failure("peer-2").await; // Opens circuit

        let states = cb.get_all_states().await;
        assert_eq!(states.get("peer-1"), Some(&CircuitState::Closed));
        assert_eq!(states.get("peer-2"), Some(&CircuitState::Open));
    }

    #[tokio::test]
    async fn test_should_allow_sync_closed() {
        let cb = CircuitBreaker::new(CircuitBreakerConfig::default());
        cb.should_allow("peer-1").await; // Initialize
        assert!(cb.should_allow_sync("peer-1"));
    }

    #[tokio::test]
    async fn test_should_allow_sync_open() {
        let config = CircuitBreakerConfig {
            failure_threshold: 1,
            open_duration_base_ms: 60000, // Long backoff
            ..Default::default()
        };
        let cb = CircuitBreaker::new(config);

        cb.record_failure("peer-1").await;
        assert!(!cb.should_allow_sync("peer-1")); // Blocked
    }

    #[tokio::test]
    async fn test_should_allow_sync_unknown_peer() {
        let cb = CircuitBreaker::new(CircuitBreakerConfig::default());
        assert!(cb.should_allow_sync("unknown-peer")); // Unknown = closed
    }

    #[test]
    fn test_backoff_zero_consecutive_opens() {
        let cb = CircuitBreaker::new(CircuitBreakerConfig {
            open_duration_base_ms: 1000,
            ..Default::default()
        });
        // Edge case: 0 consecutive opens (shouldn't happen but handle gracefully)
        assert_eq!(cb.calculate_backoff(0), Duration::from_millis(1000));
    }

    #[test]
    fn test_backoff_overflow_protection() {
        let cb = CircuitBreaker::new(CircuitBreakerConfig {
            open_duration_base_ms: 1000,
            open_duration_max_ms: 60000,
            ..Default::default()
        });
        // Large consecutive opens shouldn't overflow
        assert_eq!(cb.calculate_backoff(100), Duration::from_millis(60000));
        assert_eq!(cb.calculate_backoff(u32::MAX), Duration::from_millis(60000));
    }
}
