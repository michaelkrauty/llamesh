//! In-flight spawn reservation tracking.
//!
//! `try_get_or_spawn` releases the `instances` write lock before spawning a
//! `llama-server` process (spawning is slow and must not block the node), and
//! only inserts the instance into the map afterwards. Without reservations,
//! two concurrent requests for the same profile could both pass the capacity
//! checks inside that window, both spawn, and the loser would have to kill
//! its just-spawned process at insertion time — a wasted process exec and
//! partial model load.
//!
//! A reservation records "a spawn is in flight" so capacity checks can count
//! it before the instance reaches the map. Reservations are created while the
//! caller holds the `instances` write lock, which makes check-and-reserve
//! atomic: the second contender observes the first's reservation and queues
//! instead of spawning.
//!
//! The interior mutex is a synchronous leaf lock, held only for the map
//! operation itself and never across `await`. Releases happen from `Drop`,
//! so failure paths and future cancellation cannot leak a reservation.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::Arc;

/// Counts of spawns that have passed capacity checks but whose instances are
/// not yet in the instances map, keyed by `model:profile`.
#[derive(Default)]
pub struct SpawnReservations {
    by_profile: Mutex<HashMap<String, usize>>,
}

impl SpawnReservations {
    /// Number of in-flight spawns for a `model:profile` key.
    pub fn profile_count(&self, key: &str) -> usize {
        self.by_profile.lock().get(key).copied().unwrap_or(0)
    }

    /// Total in-flight spawns on this node.
    pub fn node_total(&self) -> usize {
        self.by_profile.lock().values().sum()
    }

    /// Records an in-flight spawn and returns its guard.
    ///
    /// Call only while holding the `instances` write lock, so that the
    /// capacity check and the reservation form one atomic step with respect
    /// to other spawners.
    pub fn reserve(self: &Arc<Self>, key: String) -> SpawnReservation {
        *self.by_profile.lock().entry(key.clone()).or_insert(0) += 1;
        SpawnReservation {
            reservations: self.clone(),
            key,
            released: false,
        }
    }

    fn release_one(&self, key: &str) {
        let mut by_profile = self.by_profile.lock();
        match by_profile.get_mut(key) {
            Some(n) if *n > 1 => *n -= 1,
            Some(_) => {
                by_profile.remove(key);
            }
            None => debug_assert!(false, "released a reservation that was never taken: {key}"),
        }
    }
}

/// RAII guard for one reserved spawn.
///
/// Dropping the guard releases the reservation, which covers every error
/// path and future cancellation. When the spawned instance has been inserted
/// into the instances map, call [`SpawnReservation::release`] at the
/// insertion site (while still holding the `instances` write lock): from that
/// moment the instance is counted via the map, and keeping the reservation
/// would double-count it.
pub struct SpawnReservation {
    reservations: Arc<SpawnReservations>,
    key: String,
    released: bool,
}

impl SpawnReservation {
    /// Releases the reservation. Idempotent; `Drop` becomes a no-op after.
    pub fn release(&mut self) {
        if !self.released {
            self.released = true;
            self.reservations.release_one(&self.key);
        }
    }
}

impl Drop for SpawnReservation {
    fn drop(&mut self) {
        self.release();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reserve_and_drop_round_trips() {
        let reservations = Arc::new(SpawnReservations::default());
        assert_eq!(reservations.profile_count("m:p"), 0);
        assert_eq!(reservations.node_total(), 0);

        let guard = reservations.reserve("m:p".to_string());
        assert_eq!(reservations.profile_count("m:p"), 1);
        assert_eq!(reservations.node_total(), 1);

        drop(guard);
        assert_eq!(reservations.profile_count("m:p"), 0);
        assert_eq!(reservations.node_total(), 0);
    }

    #[test]
    fn explicit_release_makes_drop_a_no_op() {
        let reservations = Arc::new(SpawnReservations::default());
        let mut guard = reservations.reserve("m:p".to_string());
        guard.release();
        assert_eq!(reservations.profile_count("m:p"), 0);
        guard.release();
        assert_eq!(reservations.profile_count("m:p"), 0);
        drop(guard);
        assert_eq!(reservations.profile_count("m:p"), 0);
        assert_eq!(reservations.node_total(), 0);
    }

    #[test]
    fn counts_are_per_key_and_stack() {
        let reservations = Arc::new(SpawnReservations::default());
        let g1 = reservations.reserve("m:a".to_string());
        let g2 = reservations.reserve("m:a".to_string());
        let g3 = reservations.reserve("m:b".to_string());

        assert_eq!(reservations.profile_count("m:a"), 2);
        assert_eq!(reservations.profile_count("m:b"), 1);
        assert_eq!(reservations.node_total(), 3);

        drop(g1);
        assert_eq!(reservations.profile_count("m:a"), 1);
        assert_eq!(reservations.node_total(), 2);

        drop(g2);
        drop(g3);
        assert_eq!(reservations.profile_count("m:a"), 0);
        assert_eq!(reservations.profile_count("m:b"), 0);
        assert_eq!(reservations.node_total(), 0);
    }
}
