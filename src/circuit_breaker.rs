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
use std::sync::atomic::{AtomicU64, Ordering};
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
    /// Creation instant; epoch for `last_probe_ms`.
    created: Instant,
    /// Milliseconds since `created` of the most recently admitted probe
    /// (0 = never). Atomic so the read-lock-only sync gate can claim probes
    /// without taking the circuits write lock.
    last_probe_ms: AtomicU64,
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
            created: Instant::now(),
            last_probe_ms: AtomicU64::new(0),
        }
    }
}

impl PeerCircuit {
    /// Try to claim a recovery probe slot. At most one probe is admitted per
    /// `interval_ms`, so a recovering peer sees a trickle of probes instead
    /// of the full queued backlog. An interval of 0 disables gating (every
    /// request is admitted, the pre-gating behavior).
    fn try_claim_probe(&self, interval_ms: u64) -> bool {
        if interval_ms == 0 {
            return true;
        }
        // `max(1)` keeps a probe claimed in the first millisecond
        // distinguishable from "never probed".
        let now_ms = (self.created.elapsed().as_millis() as u64).max(1);
        let last = self.last_probe_ms.load(Ordering::Relaxed);
        if last != 0 && now_ms.saturating_sub(last) < interval_ms {
            return false;
        }
        // CAS so concurrent claimants admit exactly one probe.
        self.last_probe_ms
            .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    }

    /// Forget probe history (on state transitions that reset counters).
    fn reset_probe(&self) {
        self.last_probe_ms.store(0, Ordering::Relaxed);
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

    /// Minimum interval in ms between recovery probes once an open circuit's
    /// backoff has elapsed (default: 1000). Limits how much traffic a
    /// recovering peer sees before the circuit closes. 0 disables gating —
    /// every request is admitted once the backoff has elapsed.
    #[serde(default = "default_half_open_probe_interval_ms")]
    pub half_open_probe_interval_ms: u64,

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
fn default_half_open_probe_interval_ms() -> u64 {
    1000
}

impl Default for CircuitBreakerConfig {
    fn default() -> Self {
        Self {
            failure_threshold: default_failure_threshold(),
            success_threshold: default_success_threshold(),
            open_duration_base_ms: default_open_duration_base_ms(),
            open_duration_max_ms: default_open_duration_max_ms(),
            half_open_probe_interval_ms: default_half_open_probe_interval_ms(),
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
                    circuit.reset_probe();
                    tracing::info!(
                        peer_id = %peer_id,
                        "Circuit breaker transitioning to half-open"
                    );
                    // Claim the first recovery probe.
                    circuit.try_claim_probe(self.config.half_open_probe_interval_ms)
                } else {
                    false
                }
            }
            CircuitState::HalfOpen => {
                // Admit one recovery probe per configured interval.
                circuit.try_claim_probe(self.config.half_open_probe_interval_ms)
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
            // Half-open (explicit, or implicit as an open circuit whose
            // backoff has elapsed): admit one recovery probe per configured
            // interval so the recovering peer is not herded by the whole
            // queued backlog. The probe stamp is an atomic, so this works
            // under the read lock.
            CircuitState::HalfOpen => {
                circuit.try_claim_probe(self.config.half_open_probe_interval_ms)
            }
            CircuitState::Open => {
                let backoff = self.calculate_backoff(circuit.consecutive_opens);
                if circuit.last_state_change.elapsed() < backoff {
                    return false;
                }
                circuit.try_claim_probe(self.config.half_open_probe_interval_ms)
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
                    circuit.reset_probe();
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
                // The production gate (`should_allow_sync`) admits probes
                // once the backoff has elapsed but cannot transition state
                // under its read lock — a success arriving here is a
                // successful recovery probe, so drive the transition now.
                // (A success while the backoff is still running is a stale
                // response from a request sent before the circuit opened;
                // ignore it, as before.)
                let backoff = self.calculate_backoff(circuit.consecutive_opens);
                if circuit.last_state_change.elapsed() >= backoff {
                    circuit.last_state_change = Instant::now();
                    circuit.success_count = 1;
                    if circuit.success_count >= self.config.success_threshold {
                        circuit.state = CircuitState::Closed;
                        circuit.failure_count = 0;
                        circuit.consecutive_opens = 0;
                        circuit.reset_probe();
                        tracing::info!(
                            peer_id = %peer_id,
                            "Circuit breaker closed after successful recovery"
                        );
                    } else {
                        circuit.state = CircuitState::HalfOpen;
                        tracing::info!(
                            peer_id = %peer_id,
                            "Circuit breaker transitioning to half-open"
                        );
                    }
                }
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
                circuit.success_count = 0;
                circuit.reset_probe();
                tracing::warn!(
                    peer_id = %peer_id,
                    "Circuit breaker reopened after failure in half-open state"
                );
            }
            CircuitState::Open => {
                // A failure after the backoff elapsed is a failed recovery
                // probe admitted by the sync gate: restart the block window
                // with escalated backoff. Without this, the first backoff
                // expiry permanently un-gates a still-failing peer, since
                // nothing ever moves `last_state_change` forward again.
                // (Failures inside the window are stale responses from
                // requests sent before the circuit opened; keep waiting.)
                let backoff = self.calculate_backoff(circuit.consecutive_opens);
                if circuit.last_state_change.elapsed() >= backoff {
                    circuit.last_state_change = Instant::now();
                    circuit.consecutive_opens += 1;
                    circuit.success_count = 0;
                    circuit.reset_probe();
                    tracing::warn!(
                        peer_id = %peer_id,
                        consecutive_opens = circuit.consecutive_opens,
                        "Circuit breaker reopened after failed recovery probe"
                    );
                }
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
    async fn sync_gate_drives_recovery_through_record_paths() {
        // The production path is should_allow_sync (read lock, no state
        // transitions) plus record_success/record_failure — recovery must
        // work through those alone.
        let config = CircuitBreakerConfig {
            failure_threshold: 2,
            success_threshold: 2,
            open_duration_base_ms: 1,
            half_open_probe_interval_ms: 0, // gating off; tested separately
            ..Default::default()
        };
        let cb = CircuitBreaker::new(config);

        cb.record_failure("p").await;
        cb.record_failure("p").await;
        assert_eq!(cb.get_state("p").await, CircuitState::Open);
        assert!(!cb.should_allow_sync("p"));

        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(cb.should_allow_sync("p")); // probe admitted after backoff

        cb.record_success("p").await; // successful probe → half-open
        assert_eq!(cb.get_state("p").await, CircuitState::HalfOpen);
        cb.record_success("p").await; // second success → closed
        assert_eq!(cb.get_state("p").await, CircuitState::Closed);
    }

    #[tokio::test]
    async fn failed_probe_reopens_with_escalated_backoff() {
        let config = CircuitBreakerConfig {
            failure_threshold: 1,
            open_duration_base_ms: 40,
            open_duration_max_ms: 10_000,
            half_open_probe_interval_ms: 0,
            ..Default::default()
        };
        let cb = CircuitBreaker::new(config);

        cb.record_failure("p").await; // open; backoff 40ms
        assert!(!cb.should_allow_sync("p"));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(cb.should_allow_sync("p")); // probe admitted

        // Failed probe must restart the block window with doubled backoff;
        // without it, the first expiry permanently un-gates the peer.
        cb.record_failure("p").await;
        assert_eq!(cb.get_state("p").await, CircuitState::Open);
        assert!(!cb.should_allow_sync("p"));
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(!cb.should_allow_sync("p")); // still inside the 80ms backoff
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(cb.should_allow_sync("p")); // escalated backoff elapsed
    }

    #[tokio::test]
    async fn stale_results_inside_backoff_window_do_not_change_state() {
        let config = CircuitBreakerConfig {
            failure_threshold: 1,
            open_duration_base_ms: 60_000,
            ..Default::default()
        };
        let cb = CircuitBreaker::new(config);
        cb.record_failure("p").await;
        assert_eq!(cb.get_state("p").await, CircuitState::Open);

        // Late results from requests sent before the circuit opened must
        // neither recover nor escalate the circuit.
        cb.record_success("p").await;
        assert_eq!(cb.get_state("p").await, CircuitState::Open);
        cb.record_failure("p").await;
        assert_eq!(cb.get_state("p").await, CircuitState::Open);
        assert!(!cb.should_allow_sync("p"));
    }

    #[tokio::test]
    async fn probe_gating_limits_admissions_per_interval() {
        let config = CircuitBreakerConfig {
            failure_threshold: 1,
            success_threshold: 2,
            open_duration_base_ms: 1,
            half_open_probe_interval_ms: 60_000,
            ..Default::default()
        };
        let cb = CircuitBreaker::new(config);
        cb.record_failure("p").await;
        tokio::time::sleep(Duration::from_millis(5)).await;

        assert!(cb.should_allow_sync("p")); // first probe admitted
        assert!(!cb.should_allow_sync("p")); // backlog herd denied
        assert!(!cb.should_allow_sync("p"));

        // Recovery through probe successes reopens unrestricted traffic.
        cb.record_success("p").await;
        cb.record_success("p").await;
        assert_eq!(cb.get_state("p").await, CircuitState::Closed);
        assert!(cb.should_allow_sync("p"));
        assert!(cb.should_allow_sync("p"));
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
