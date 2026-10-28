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

/// Outcome of a dispatch admission check against a peer's circuit.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DispatchDecision {
    /// Send the request. `probe_ticket` is true when this dispatch is a
    /// recovery probe for an open/half-open circuit: the caller must pass it
    /// to `record_success`/`record_failure` so the result is attributed to
    /// this specific dispatch. Results without a ticket never drive
    /// recovery, which keeps late responses from requests sent before the
    /// circuit opened from recovering (or re-blocking) a peer.
    Admit { probe_ticket: bool },
    /// Do not send: the circuit is blocking, or the recovery probe slot for
    /// this interval is already taken.
    Deny,
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
    /// When the circuit last left the closed state. Meaningful only while it
    /// is open or half-open; reopens do not move it.
    opened_at: Instant,
    /// Creation instant; epoch for `last_probe_ms`.
    created: Instant,
    /// Milliseconds since `created` of the most recently admitted probe
    /// (0 = never). Paces probe admission. Atomic so the read-lock-only sync
    /// gate can claim probes without taking the circuits write lock.
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
            opened_at: Instant::now(),
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
        // `max(1)` keeps a probe claimed in the first millisecond
        // distinguishable from "never probed".
        let now_ms = (self.created.elapsed().as_millis() as u64).max(1);
        if interval_ms == 0 {
            // Gating disabled: every dispatch is admitted as a probe.
            self.last_probe_ms.store(now_ms, Ordering::Relaxed);
            return true;
        }
        let last = self.last_probe_ms.load(Ordering::Relaxed);
        if last != 0 && now_ms.saturating_sub(last) < interval_ms {
            return false;
        }
        // CAS so concurrent claimants admit exactly one probe per interval.
        self.last_probe_ms
            .compare_exchange(last, now_ms, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    }

    /// Forget probe pacing (on state transitions that reset counters).
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
            CircuitState::HalfOpen => true,
            CircuitState::Open => {
                // Eligible for recovery probes once the backoff has elapsed.
                // This check is read-only on purpose: it runs while routing
                // filters peer candidates, and most filtered candidates are
                // never dispatched to. The probe slot is claimed at the point
                // of committing to send, via `try_claim_dispatch_sync`.
                let backoff = self.calculate_backoff(circuit.consecutive_opens);
                circuit.last_state_change.elapsed() >= backoff
            }
        }
    }

    /// Claim the right to dispatch one request to this peer; call at the
    /// point of committing to send (not while filtering candidates, which
    /// would burn the probe slot on peers that are never dispatched to).
    ///
    /// Closed circuits always admit (without a probe ticket). Circuits in
    /// the recovery phase — half-open, or open with backoff elapsed — admit
    /// at most one probe per `half_open_probe_interval_ms` and hand the
    /// caller the probe ticket for result attribution, so a recovering peer
    /// sees a trickle of probes instead of the queued backlog. Like
    /// `should_allow_sync`, this is synchronous and conservatively admits
    /// when the lock is contended.
    pub fn try_claim_dispatch_sync(&self, peer_id: &str) -> DispatchDecision {
        if !self.config.enabled {
            return DispatchDecision::Admit {
                probe_ticket: false,
            };
        }

        let circuits = match self.circuits.try_read() {
            Ok(guard) => guard,
            Err(_) => {
                return DispatchDecision::Admit {
                    probe_ticket: false,
                }
            }
        };

        let Some(circuit) = circuits.get(peer_id) else {
            return DispatchDecision::Admit {
                probe_ticket: false,
            };
        };

        match circuit.state {
            CircuitState::Closed => DispatchDecision::Admit {
                probe_ticket: false,
            },
            CircuitState::HalfOpen => {
                if circuit.try_claim_probe(self.config.half_open_probe_interval_ms) {
                    DispatchDecision::Admit { probe_ticket: true }
                } else {
                    DispatchDecision::Deny
                }
            }
            CircuitState::Open => {
                let backoff = self.calculate_backoff(circuit.consecutive_opens);
                if circuit.last_state_change.elapsed() >= backoff
                    && circuit.try_claim_probe(self.config.half_open_probe_interval_ms)
                {
                    DispatchDecision::Admit { probe_ticket: true }
                } else {
                    DispatchDecision::Deny
                }
            }
        }
    }

    /// Record a successful request to a peer.
    ///
    /// Returns `true` when the success advanced a recovery transition
    /// (open → half-open, or → closed): callers should wake routing waiters
    /// parked on capacity notifications, which may have filtered this peer
    /// out before it recovered.
    pub async fn record_success(&self, peer_id: &str, probe_ticket: bool) -> bool {
        if !self.config.enabled {
            return false;
        }

        let mut circuits = self.circuits.write().await;
        let circuit = circuits.entry(peer_id.to_string()).or_default();

        match circuit.state {
            CircuitState::HalfOpen => {
                // Only results from this dispatch's claimed probe count
                // toward recovery; stale pre-open responses carry no ticket.
                if !probe_ticket {
                    return false;
                }
                circuit.success_count += 1;
                if circuit.success_count >= self.config.success_threshold {
                    Self::close_after_recovery(circuit, peer_id);
                    return true;
                }
                false
            }
            CircuitState::Closed => {
                // Reset failure count on success
                circuit.failure_count = 0;
                false
            }
            CircuitState::Open => {
                // The production gate cannot transition state under its read
                // lock, so a success from a dispatched recovery probe arrives
                // here while the circuit is still Open — drive the transition
                // now. The caller-carried ticket is the attribution: with
                // unlimited request durations, a slow response from a request
                // sent before the circuit opened can arrive at any time, and
                // carries no ticket, so it cannot recover a peer nobody has
                // re-probed.
                if probe_ticket {
                    circuit.last_state_change = Instant::now();
                    circuit.success_count = 1;
                    if circuit.success_count >= self.config.success_threshold {
                        Self::close_after_recovery(circuit, peer_id);
                    } else {
                        // Probe pacing (last_probe_ms) deliberately persists
                        // through this transition: the next half-open probe
                        // still waits out the configured interval.
                        circuit.state = CircuitState::HalfOpen;
                        tracing::info!(
                            peer_id = %peer_id,
                            "Circuit breaker transitioning to half-open"
                        );
                    }
                    return true;
                }
                false
            }
        }
    }

    /// Record a failed request to a peer. `probe_ticket` carries this
    /// dispatch's probe attribution, as in `record_success`.
    pub async fn record_failure(&self, peer_id: &str, probe_ticket: bool) {
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
                    circuit.opened_at = circuit.last_state_change;
                    circuit.consecutive_opens += 1;
                    circuit.success_count = 0;
                    circuit.reset_probe();
                    // The backoff now in effect: the circuit blocks dispatches
                    // for this long before a recovery probe is admitted. Logged
                    // so operators see the block duration without recomputing
                    // the exponential schedule from `consecutive_opens` by hand.
                    let backoff_ms = self
                        .calculate_backoff(circuit.consecutive_opens)
                        .as_millis() as u64;
                    tracing::warn!(
                        peer_id = %peer_id,
                        failures = circuit.failure_count,
                        consecutive_opens = circuit.consecutive_opens,
                        backoff_ms,
                        "Circuit breaker opened after consecutive failures"
                    );
                }
            }
            CircuitState::HalfOpen => {
                // Only a ticketed probe's failure reopens the circuit; a
                // slow failure from a request sent before the circuit opened
                // must not re-block a recovering peer.
                if probe_ticket {
                    circuit.state = CircuitState::Open;
                    circuit.last_state_change = Instant::now();
                    circuit.consecutive_opens += 1;
                    circuit.success_count = 0;
                    circuit.reset_probe();
                    let backoff_ms = self
                        .calculate_backoff(circuit.consecutive_opens)
                        .as_millis() as u64;
                    let open_for_secs = circuit.opened_at.elapsed().as_secs();
                    if self.reopen_logs_at_warn(circuit.consecutive_opens) {
                        tracing::warn!(
                            peer_id = %peer_id,
                            consecutive_opens = circuit.consecutive_opens,
                            backoff_ms,
                            open_for_secs,
                            "Circuit breaker reopened after failure in half-open state"
                        );
                    } else {
                        tracing::debug!(
                            peer_id = %peer_id,
                            consecutive_opens = circuit.consecutive_opens,
                            backoff_ms,
                            open_for_secs,
                            "Circuit breaker reopened after failure in half-open state"
                        );
                    }
                }
            }
            CircuitState::Open => {
                // A failure from a ticketed recovery probe restarts the block
                // window with escalated backoff. Without this, the first
                // backoff expiry permanently un-gates a still-failing peer,
                // since nothing ever moves `last_state_change` forward again.
                // The ticket requirement keeps stale failures — slow
                // requests sent before the circuit opened — from escalating
                // a window nobody has re-probed.
                if probe_ticket {
                    circuit.last_state_change = Instant::now();
                    circuit.consecutive_opens += 1;
                    circuit.success_count = 0;
                    circuit.reset_probe();
                    let backoff_ms = self
                        .calculate_backoff(circuit.consecutive_opens)
                        .as_millis() as u64;
                    let open_for_secs = circuit.opened_at.elapsed().as_secs();
                    if self.reopen_logs_at_warn(circuit.consecutive_opens) {
                        tracing::warn!(
                            peer_id = %peer_id,
                            consecutive_opens = circuit.consecutive_opens,
                            backoff_ms,
                            open_for_secs,
                            "Circuit breaker reopened after failed recovery probe"
                        );
                    } else {
                        tracing::debug!(
                            peer_id = %peer_id,
                            consecutive_opens = circuit.consecutive_opens,
                            backoff_ms,
                            open_for_secs,
                            "Circuit breaker reopened after failed recovery probe"
                        );
                    }
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

        // Exponential backoff: base * 2^(consecutive_opens - 1), capped at max.
        // `saturating_mul` guards the multiply: with a large (but valid, <= max)
        // base, `base * 1024` could otherwise overflow u64 and wrap to a tiny
        // value that slips under the `.min(max)` cap, collapsing the backoff.
        let exponent = consecutive_opens.saturating_sub(1).min(10);
        let backoff_ms = base.saturating_mul(2u64.pow(exponent)).min(max);

        Duration::from_millis(backoff_ms)
    }

    /// Whether the reopen that brought the circuit to `consecutive_opens` is
    /// logged at WARN rather than DEBUG. A peer that stays down fails a
    /// recovery probe once per backoff for as long as it is down — at the
    /// default cap, once a minute — so logging every reopen at WARN buries
    /// other warnings under one line per minute of the outage. Reopens that
    /// lengthen the backoff are logged at WARN; once it stops growing, only
    /// the 2^k-th reopens are, so the reminders thin out as the outage goes
    /// on and an outage of n reopens logs O(log n) WARN lines.
    fn reopen_logs_at_warn(&self, consecutive_opens: u32) -> bool {
        consecutive_opens.is_power_of_two()
            || self.calculate_backoff(consecutive_opens)
                != self.calculate_backoff(consecutive_opens.saturating_sub(1))
    }

    /// Close a recovering circuit, reporting how long it was open.
    fn close_after_recovery(circuit: &mut PeerCircuit, peer_id: &str) {
        let consecutive_opens = circuit.consecutive_opens;
        circuit.state = CircuitState::Closed;
        circuit.last_state_change = Instant::now();
        circuit.failure_count = 0;
        circuit.consecutive_opens = 0;
        circuit.reset_probe();
        tracing::info!(
            peer_id = %peer_id,
            consecutive_opens,
            open_for_secs = circuit.opened_at.elapsed().as_secs(),
            "Circuit breaker closed after successful recovery"
        );
    }

    /// Remove the named circuits without logging a manual reset, returning
    /// how many existed. Keys without a circuit are ignored.
    pub async fn forget<I, S>(&self, keys: I) -> usize
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let keys: Vec<S> = keys.into_iter().collect();
        // Callers may name keys that are already gone on most calls. Check
        // under the read lock first: while the write lock is held, the
        // synchronous routing checks cannot read the map and admit the
        // request instead.
        {
            let circuits = self.circuits.read().await;
            if !keys.iter().any(|key| circuits.contains_key(key.as_ref())) {
                return 0;
            }
        }
        let mut circuits = self.circuits.write().await;
        let mut removed = 0;
        for key in keys {
            let peer_id = key.as_ref();
            if circuits.remove(peer_id).is_some() {
                removed += 1;
                tracing::debug!(peer_id = %peer_id, "Circuit breaker entry forgotten");
            }
        }
        removed
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
    async fn test_forget_removes_only_named_keys() {
        let cb = CircuitBreaker::new(CircuitBreakerConfig {
            failure_threshold: 1,
            ..Default::default()
        });
        cb.record_failure("remove-open", false).await;
        cb.record_success("remove-closed", false).await;
        cb.record_failure("keep-open", false).await;
        cb.record_success("keep-closed", false).await;

        assert_eq!(
            cb.forget(["remove-open", "remove-closed", "remove-open", "unknown"])
                .await,
            2
        );
        let expected = HashMap::from([
            ("keep-open".to_string(), CircuitState::Open),
            ("keep-closed".to_string(), CircuitState::Closed),
        ]);
        assert_eq!(cb.get_all_states().await, expected);
        assert_eq!(cb.forget(["unknown", "remove-open"]).await, 0);
        assert_eq!(cb.get_all_states().await, expected);
    }

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

        cb.record_failure("peer-1", false).await;
        cb.record_failure("peer-1", false).await;
        assert!(cb.should_allow("peer-1").await); // Still closed

        cb.record_failure("peer-1", false).await;
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

        cb.record_failure("peer-1", false).await;
        cb.record_failure("peer-1", false).await;
        cb.record_success("peer-1", false).await; // Reset

        cb.record_failure("peer-1", false).await;
        cb.record_failure("peer-1", false).await;
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

        cb.record_failure("peer-1", false).await;
        cb.record_failure("peer-1", false).await;
        cb.record_failure("peer-1", false).await;

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

    #[test]
    fn test_backoff_saturates_instead_of_overflowing() {
        // A large (but valid: base <= max) base must not overflow the
        // `base * 2^exponent` multiply and wrap to a tiny value that slips
        // under the cap. `1 << 54` times `2^10` is exactly `1 << 64`, which
        // wraps to 0 with a plain multiply; saturating multiplication clamps,
        // and `.min(max)` then yields the configured maximum.
        let cb = CircuitBreaker::new(CircuitBreakerConfig {
            open_duration_base_ms: 1u64 << 54,
            open_duration_max_ms: 1u64 << 60,
            ..Default::default()
        });
        // The exponent caps at 10, so this is (1 << 54) * (1 << 10) = 1 << 64.
        assert_eq!(
            cb.calculate_backoff(11),
            Duration::from_millis(1u64 << 60),
            "backoff must saturate to max, not wrap to ~0"
        );
    }

    #[tokio::test]
    async fn test_half_open_to_closed_on_success() {
        let config = CircuitBreakerConfig {
            failure_threshold: 2,
            success_threshold: 2,
            open_duration_base_ms: 1,       // 1ms for fast test
            half_open_probe_interval_ms: 0, // every probe admitted
            ..Default::default()
        };
        let cb = CircuitBreaker::new(config);

        // Open the circuit
        cb.record_failure("peer-1", false).await;
        cb.record_failure("peer-1", false).await;
        assert_eq!(cb.get_state("peer-1").await, CircuitState::Open);

        // Wait for backoff and trigger half-open
        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(cb.should_allow("peer-1").await); // Transitions to HalfOpen, claims probe
        assert_eq!(cb.get_state("peer-1").await, CircuitState::HalfOpen);

        // Ticketed probe successes in half-open close the circuit
        cb.record_success("peer-1", true).await;
        assert_eq!(cb.get_state("peer-1").await, CircuitState::HalfOpen);
        assert!(cb.should_allow("peer-1").await); // admit the next probe
        cb.record_success("peer-1", true).await;
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
        cb.record_failure("peer-1", false).await;
        cb.record_failure("peer-1", false).await;
        assert_eq!(cb.get_state("peer-1").await, CircuitState::Open);

        // Wait and transition to half-open
        tokio::time::sleep(Duration::from_millis(5)).await;
        cb.should_allow("peer-1").await;
        assert_eq!(cb.get_state("peer-1").await, CircuitState::HalfOpen);

        // Ticketed probe failure in half-open reopens circuit
        cb.record_failure("peer-1", true).await;
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
        cb.record_failure("peer-1", false).await;
        cb.record_failure("peer-1", false).await;
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
        cb.record_failure("peer-2", false).await; // Opens circuit

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

        cb.record_failure("p", false).await;
        cb.record_failure("p", false).await;
        assert_eq!(cb.get_state("p").await, CircuitState::Open);
        assert!(!cb.should_allow_sync("p"));

        tokio::time::sleep(Duration::from_millis(5)).await;
        assert!(cb.should_allow_sync("p")); // eligible after backoff
        assert!(matches!(
            cb.try_claim_dispatch_sync("p"),
            DispatchDecision::Admit { probe_ticket: true }
        )); // probe claimed at dispatch

        assert!(cb.record_success("p", true).await); // successful probe → half-open
        assert_eq!(cb.get_state("p").await, CircuitState::HalfOpen);
        assert!(matches!(
            cb.try_claim_dispatch_sync("p"),
            DispatchDecision::Admit { probe_ticket: true }
        )); // next probe claimed
        assert!(cb.record_success("p", true).await); // second success → closed
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

        cb.record_failure("p", false).await; // open; backoff 40ms
        assert!(!cb.should_allow_sync("p"));
        assert!(matches!(
            cb.try_claim_dispatch_sync("p"),
            DispatchDecision::Deny
        )); // dispatch denied too
        tokio::time::sleep(Duration::from_millis(50)).await;
        assert!(cb.should_allow_sync("p")); // eligible
        assert!(matches!(
            cb.try_claim_dispatch_sync("p"),
            DispatchDecision::Admit { probe_ticket: true }
        )); // probe claimed

        // Failed probe must restart the block window with doubled backoff;
        // without it, the first expiry permanently un-gates the peer.
        cb.record_failure("p", true).await;
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
        cb.record_failure("p", false).await;
        assert_eq!(cb.get_state("p").await, CircuitState::Open);

        // Late results from requests sent before the circuit opened must
        // neither recover nor escalate the circuit.
        cb.record_success("p", false).await;
        assert_eq!(cb.get_state("p").await, CircuitState::Open);
        cb.record_failure("p", false).await;
        assert_eq!(cb.get_state("p").await, CircuitState::Open);
        assert!(!cb.should_allow_sync("p"));
    }

    #[tokio::test]
    async fn probe_gating_limits_dispatch_claims_per_interval() {
        let config = CircuitBreakerConfig {
            failure_threshold: 1,
            success_threshold: 1,
            open_duration_base_ms: 1,
            half_open_probe_interval_ms: 60_000,
            ..Default::default()
        };
        let cb = CircuitBreaker::new(config);
        cb.record_failure("p", false).await;
        tokio::time::sleep(Duration::from_millis(5)).await;

        // Eligibility checks (candidate filtering) are read-only and never
        // consume the probe slot, no matter how many scans run.
        assert!(cb.should_allow_sync("p"));
        assert!(cb.should_allow_sync("p"));

        assert!(matches!(
            cb.try_claim_dispatch_sync("p"),
            DispatchDecision::Admit { probe_ticket: true }
        )); // first dispatch claims the probe
        assert!(matches!(
            cb.try_claim_dispatch_sync("p"),
            DispatchDecision::Deny
        )); // backlog herd denied
        assert!(matches!(
            cb.try_claim_dispatch_sync("p"),
            DispatchDecision::Deny
        ));

        // The probe's success closes the circuit and reopens unrestricted
        // traffic (closed-circuit admissions carry no probe ticket).
        assert!(cb.record_success("p", true).await);
        assert_eq!(cb.get_state("p").await, CircuitState::Closed);
        assert!(matches!(
            cb.try_claim_dispatch_sync("p"),
            DispatchDecision::Admit {
                probe_ticket: false
            }
        ));
        assert!(matches!(
            cb.try_claim_dispatch_sync("p"),
            DispatchDecision::Admit {
                probe_ticket: false
            }
        ));
    }

    #[tokio::test]
    async fn probe_pacing_persists_across_half_open_transition() {
        let config = CircuitBreakerConfig {
            failure_threshold: 1,
            success_threshold: 2,
            open_duration_base_ms: 1,
            half_open_probe_interval_ms: 60_000,
            ..Default::default()
        };
        let cb = CircuitBreaker::new(config);
        cb.record_failure("p", false).await;
        tokio::time::sleep(Duration::from_millis(5)).await;

        assert!(matches!(
            cb.try_claim_dispatch_sync("p"),
            DispatchDecision::Admit { probe_ticket: true }
        ));
        assert!(cb.record_success("p", true).await); // probe success → half-open
        assert_eq!(cb.get_state("p").await, CircuitState::HalfOpen);

        // The interval still paces half-open probes after the transition: a
        // woken backlog cannot immediately claim the next probe.
        assert!(matches!(
            cb.try_claim_dispatch_sync("p"),
            DispatchDecision::Deny
        ));

        // And stale results without a claimed probe neither advance recovery
        // nor re-block the recovering peer in half-open.
        assert!(!cb.record_success("p", false).await);
        assert_eq!(cb.get_state("p").await, CircuitState::HalfOpen);
        cb.record_failure("p", false).await;
        assert_eq!(cb.get_state("p").await, CircuitState::HalfOpen);
    }

    #[tokio::test]
    async fn unclaimed_results_after_backoff_do_not_change_state() {
        // With unlimited request durations, a slow response from a request
        // sent before the circuit opened can arrive after the backoff has
        // elapsed. Without a claimed probe it must neither recover the
        // circuit nor escalate the backoff.
        let config = CircuitBreakerConfig {
            failure_threshold: 1,
            open_duration_base_ms: 1,
            ..Default::default()
        };
        let cb = CircuitBreaker::new(config);
        cb.record_failure("p", false).await;
        tokio::time::sleep(Duration::from_millis(5)).await;

        assert!(!cb.record_success("p", false).await); // stale success: no recovery
        assert_eq!(cb.get_state("p").await, CircuitState::Open);

        cb.record_failure("p", false).await; // stale failure: no escalation
        assert_eq!(cb.get_state("p").await, CircuitState::Open);
        // Backoff window unchanged (1ms, long elapsed): peer still eligible.
        assert!(cb.should_allow_sync("p"));

        // A ticketed probe's success, by contrast, recovers the circuit.
        assert!(matches!(
            cb.try_claim_dispatch_sync("p"),
            DispatchDecision::Admit { probe_ticket: true }
        ));
        assert!(cb.record_success("p", true).await);
        assert_eq!(cb.get_state("p").await, CircuitState::HalfOpen);
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

        cb.record_failure("peer-1", false).await;
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

    #[tokio::test]
    async fn open_log_reports_effective_backoff() {
        let capture = crate::test_support::capture_logs(tracing::Level::WARN);

        let cb = CircuitBreaker::new(CircuitBreakerConfig {
            failure_threshold: 1,
            open_duration_base_ms: 5000,
            open_duration_max_ms: 60000,
            ..Default::default()
        });

        // First open: consecutive_opens = 1 -> backoff 5000 * 2^0 = 5000ms.
        cb.record_failure("p", false).await;
        // A ticketed probe failure while open escalates the block: this is the
        // same reopen path a real failed recovery probe drives.
        // consecutive_opens = 2 -> backoff 5000 * 2^1 = 10000ms.
        cb.record_failure("p", true).await;

        let logged = capture.contents();
        assert!(
            logged.contains("\"backoff_ms\":5000"),
            "expected first open to log backoff_ms=5000; got: {logged}"
        );
        assert!(
            logged.contains("\"backoff_ms\":10000"),
            "expected escalated reopen to log backoff_ms=10000; got: {logged}"
        );
    }

    /// The captured JSON log lines whose message is `message`.
    fn logged_events(logged: &str, message: &str) -> Vec<serde_json::Value> {
        logged
            .lines()
            .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
            .filter(|event| event["fields"]["message"] == message)
            .collect()
    }

    /// `consecutive_opens` of each event logged at `level`.
    fn opens_at_level(events: &[serde_json::Value], level: &str) -> Vec<u64> {
        events
            .iter()
            .filter(|event| event["level"] == level)
            .map(|event| event["fields"]["consecutive_opens"].as_u64().unwrap())
            .collect()
    }

    #[tokio::test]
    async fn failed_probe_reopens_warn_while_backoff_grows_then_at_powers_of_two() {
        let capture = crate::test_support::capture_logs(tracing::Level::DEBUG);
        let cb = CircuitBreaker::new(CircuitBreakerConfig {
            failure_threshold: 1,
            open_duration_base_ms: 5000,
            open_duration_max_ms: 60000,
            ..Default::default()
        });

        cb.record_failure("p", false).await; // opens: consecutive_opens = 1
        for _ in 2..=64 {
            cb.record_failure("p", true).await; // failed recovery probe
        }

        let reopens = logged_events(
            &capture.contents(),
            "Circuit breaker reopened after failed recovery probe",
        );
        assert_eq!(reopens.len(), 63, "every reopen is still logged");
        // Backoffs 10s, 20s, 40s, then the 60s cap; past the cap, only the
        // 2^k-th reopens warn and the rest are logged at DEBUG.
        assert_eq!(
            opens_at_level(&reopens, "WARN"),
            vec![2, 3, 4, 5, 8, 16, 32, 64]
        );
        assert_eq!(opens_at_level(&reopens, "DEBUG").len(), 55);
    }

    #[tokio::test]
    async fn half_open_reopens_at_fixed_backoff_warn_at_powers_of_two() {
        let capture = crate::test_support::capture_logs(tracing::Level::DEBUG);
        // base == max: the backoff never grows, so only the 2^k-th warn.
        let cb = CircuitBreaker::new(CircuitBreakerConfig {
            failure_threshold: 1,
            success_threshold: 2,
            open_duration_base_ms: 60000,
            open_duration_max_ms: 60000,
            ..Default::default()
        });

        cb.record_failure("p", false).await; // opens: consecutive_opens = 1
        for _ in 2..=9 {
            assert!(cb.record_success("p", true).await); // probe succeeds: half-open
            cb.record_failure("p", true).await; // next probe fails: reopens
        }

        let reopens = logged_events(
            &capture.contents(),
            "Circuit breaker reopened after failure in half-open state",
        );
        assert_eq!(reopens.len(), 8, "every reopen is still logged");
        assert_eq!(opens_at_level(&reopens, "WARN"), vec![2, 4, 8]);
    }

    #[tokio::test]
    async fn outage_start_survives_reopens_and_recovery_reports_it() {
        // Both close paths: from open (success_threshold 1) and from
        // half-open (success_threshold 2).
        for success_threshold in [1, 2] {
            let capture = crate::test_support::capture_logs(tracing::Level::INFO);
            let cb = CircuitBreaker::new(CircuitBreakerConfig {
                failure_threshold: 1,
                success_threshold,
                ..Default::default()
            });
            let opened_at = |cb: &CircuitBreaker| {
                let circuits = cb.circuits.try_read().unwrap();
                circuits.get("p").unwrap().opened_at
            };

            cb.record_failure("p", false).await; // opens
            let first_opened = opened_at(&cb);
            cb.record_failure("p", true).await; // reopens
            cb.record_failure("p", true).await; // reopens
            assert_eq!(
                opened_at(&cb),
                first_opened,
                "reopens keep the outage start"
            );

            for _ in 0..success_threshold {
                assert!(cb.record_success("p", true).await);
            }
            assert_eq!(cb.get_state("p").await, CircuitState::Closed);

            let closed = logged_events(
                &capture.contents(),
                "Circuit breaker closed after successful recovery",
            );
            assert_eq!(closed.len(), 1);
            assert_eq!(closed[0]["fields"]["consecutive_opens"], 3);
            assert!(closed[0]["fields"]["open_for_secs"].is_u64());

            // The next outage starts its own clock.
            cb.record_failure("p", false).await;
            assert!(opened_at(&cb) > first_opened);
        }
    }
}
