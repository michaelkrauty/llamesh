//! In-flight spawn reservation tracking.
//!
//! A reservation is created while the caller holds the instances-map write
//! lock, making capacity admission and reservation atomic. The guard owns the
//! pending admission; a separate [`MemoryReservation`] can outlive that guard
//! while the spawned process loads and tears down.

use parking_lot::Mutex;
use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// One process-memory commitment tracked by [`SpawnReservations`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MemoryCommitment {
    pub key: String,
    pub pending: bool,
    pub pid: Option<u32>,
    pub estimate: (u64, u64),
    pub ready: bool,
}

/// A consistent view of all tracked loading and running commitments.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ReservationSnapshot {
    pub entries: HashMap<String, MemoryCommitment>,
    pub revision: u64,
}

impl ReservationSnapshot {
    /// Number of spawns still pending admission for a `model:profile` key.
    pub fn profile_count(&self, key: &str) -> usize {
        self.entries
            .values()
            .filter(|entry| entry.pending && entry.key == key)
            .count()
    }

    /// Total spawns still pending admission on this node.
    pub fn node_total(&self) -> usize {
        self.entries.values().filter(|entry| entry.pending).count()
    }
}

#[derive(Debug, Default)]
struct ReservationState {
    entries: HashMap<String, MemoryCommitment>,
    revision: u64,
}

/// Unified registry of pending spawns and their process-memory commitments.
///
/// The leaf mutex is only held for an in-memory map operation. It is never
/// held across an await, and [`with_revision`](Self::with_revision) lets a
/// caller synchronously commit related map changes only if this accounting has
/// not changed since its snapshot.
#[derive(Debug, Default)]
pub struct SpawnReservations {
    state: Mutex<ReservationState>,
}

impl SpawnReservations {
    /// Number of in-flight spawns for a `model:profile` key.
    #[cfg(test)]
    pub fn profile_count(&self, key: &str) -> usize {
        let state = self.state.lock();
        state
            .entries
            .values()
            .filter(|entry| entry.pending && entry.key == key)
            .count()
    }

    /// Total in-flight spawns on this node.
    #[cfg(test)]
    pub fn node_total(&self) -> usize {
        self.state
            .lock()
            .entries
            .values()
            .filter(|entry| entry.pending)
            .count()
    }

    /// Returns a consistent copy of the registry's accounting state.
    pub fn snapshot(&self) -> ReservationSnapshot {
        let state = self.state.lock();
        ReservationSnapshot {
            entries: state.entries.clone(),
            revision: state.revision,
        }
    }

    /// Runs `f` only if the registry has not changed since `revision`.
    ///
    /// The registry lock remains held for `f`, allowing callers to synchronously
    /// revalidate resource accounting and commit related instances-map changes.
    pub fn with_revision<T>(&self, revision: u64, f: impl FnOnce() -> T) -> Option<T> {
        let state = self.state.lock();
        if state.revision == revision {
            Some(f())
        } else {
            None
        }
    }

    /// Records an in-flight spawn and returns its admission guard.
    ///
    /// Call only while holding the instances write lock so the capacity check
    /// and this reservation form one atomic operation.
    pub fn reserve(
        self: &Arc<Self>,
        key: String,
        estimate: (u64, u64),
        on_abandon: Option<Box<dyn FnOnce() + Send>>,
    ) -> SpawnReservation {
        let id = ulid::Ulid::new().to_string();
        {
            let mut state = self.state.lock();
            let previous = state.entries.insert(
                id.clone(),
                MemoryCommitment {
                    key,
                    pending: true,
                    pid: None,
                    estimate,
                    ready: false,
                },
            );
            debug_assert!(previous.is_none(), "ULID collision in spawn reservations");
            state.revision = state.revision.wrapping_add(1);
        }

        SpawnReservation {
            memory: Some(Arc::new(MemoryReservation {
                reservations: self.clone(),
                id,
                finished: AtomicBool::new(false),
            })),
            handed_off: false,
            on_abandon,
        }
    }

    fn mark_handed_off(&self, id: &str) {
        let mut state = self.state.lock();
        if let Some(entry) = state.entries.get_mut(id) {
            if entry.pending {
                entry.pending = false;
                state.revision = state.revision.wrapping_add(1);
            }
        } else {
            debug_assert!(
                false,
                "handed off a reservation that was already released: {id}"
            );
        }
    }

    fn update_pid(&self, id: &str, pid: u32) {
        let mut state = self.state.lock();
        if let Some(entry) = state.entries.get_mut(id) {
            if entry.pid != Some(pid) {
                entry.pid = Some(pid);
                state.revision = state.revision.wrapping_add(1);
            }
        }
    }

    fn mark_ready(&self, id: &str) {
        let mut state = self.state.lock();
        if let Some(entry) = state.entries.get_mut(id) {
            if !entry.ready {
                entry.ready = true;
                state.revision = state.revision.wrapping_add(1);
            }
        }
    }

    fn release_memory(&self, id: &str) {
        let mut state = self.state.lock();
        if state.entries.remove(id).is_some() {
            state.revision = state.revision.wrapping_add(1);
        }
    }
}

/// RAII process-memory commitment.
///
/// The last owner removes the registry entry, or any owner can call
/// [`finish`](Self::finish) after confirmed process reaping.
#[derive(Debug)]
pub struct MemoryReservation {
    reservations: Arc<SpawnReservations>,
    id: String,
    finished: AtomicBool,
}

impl MemoryReservation {
    pub fn id(&self) -> &str {
        &self.id
    }

    /// Associates the commitment with its spawned process.
    pub fn set_pid(&self, pid: u32) {
        if !self.finished.load(Ordering::Acquire) {
            self.reservations.update_pid(&self.id, pid);
        }
    }

    /// Marks the associated process ready to serve requests.
    pub fn mark_ready(&self) {
        if !self.finished.load(Ordering::Acquire) {
            self.reservations.mark_ready(&self.id);
        }
    }

    /// Explicitly removes the commitment after confirmed process reaping.
    /// Idempotent so stale instance references can safely call it again.
    pub fn finish(&self) {
        if !self.finished.swap(true, Ordering::AcqRel) {
            self.reservations.release_memory(&self.id);
        }
    }
}

impl Drop for MemoryReservation {
    fn drop(&mut self) {
        self.finish();
    }
}

/// RAII guard for a spawn which has passed admission but not yet been inserted.
///
/// On [`handoff`](Self::handoff), pending admission becomes a process-memory
/// commitment owned by the caller through [`memory`](Self::memory). Otherwise
/// dropping the guard drops its memory owner before notifying waiters.
pub struct SpawnReservation {
    memory: Option<Arc<MemoryReservation>>,
    handed_off: bool,
    on_abandon: Option<Box<dyn FnOnce() + Send>>,
}

impl SpawnReservation {
    /// Returns a cloneable process-memory commitment for the spawned instance.
    pub fn memory(&self) -> Arc<MemoryReservation> {
        self.memory
            .as_ref()
            .expect("spawn reservation memory already released")
            .clone()
    }

    /// Transfers admission accounting to the associated process commitment.
    /// Idempotent. The abandonment callback is cleared because a mapped
    /// instance/reaper now owns the commitment's eventual cleanup.
    pub fn handoff(&mut self) {
        if self.handed_off {
            return;
        }
        self.handed_off = true;
        if let Some(memory) = self.memory.as_ref() {
            memory.reservations.mark_handed_off(memory.id());
        }
        self.on_abandon = None;
    }
}

impl Drop for SpawnReservation {
    fn drop(&mut self) {
        let memory = self.memory.take();
        drop(memory);
        if !self.handed_off {
            if let Some(callback) = self.on_abandon.take() {
                callback();
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    #[test]
    fn reserve_and_drop_round_trips() {
        let reservations = Arc::new(SpawnReservations::default());
        let guard = reservations.reserve("m:p".to_string(), (400, 800), None);

        assert_eq!(reservations.profile_count("m:p"), 1);
        assert_eq!(reservations.node_total(), 1);
        let snapshot = reservations.snapshot();
        assert_eq!(snapshot.profile_count("m:p"), 1);
        assert_eq!(snapshot.node_total(), 1);
        assert_eq!(snapshot.entries.len(), 1);
        assert_eq!(
            snapshot.entries.values().next().unwrap().estimate,
            (400, 800)
        );

        drop(guard);
        assert!(reservations.snapshot().entries.is_empty());
        assert_eq!(reservations.profile_count("m:p"), 0);
        assert_eq!(reservations.node_total(), 0);
    }

    #[test]
    fn handoff_clears_pending_but_memory_survives_until_last_owner_drops() {
        let reservations = Arc::new(SpawnReservations::default());
        let mut guard = reservations.reserve("m:p".to_string(), (400, 800), None);
        let memory = guard.memory();
        let id = memory.id().to_string();

        guard.handoff();
        drop(guard);

        let snapshot = reservations.snapshot();
        assert_eq!(reservations.profile_count("m:p"), 0);
        assert_eq!(reservations.node_total(), 0);
        assert_eq!(snapshot.entries[&id].key, "m:p");
        assert!(!snapshot.entries[&id].pending);

        drop(memory);
        assert!(reservations.snapshot().entries.is_empty());
    }

    #[test]
    fn memory_tracks_pid_and_readiness() {
        let reservations = Arc::new(SpawnReservations::default());
        let mut guard = reservations.reserve("m:p".to_string(), (400, 800), None);
        let memory = guard.memory();
        let id = memory.id().to_string();

        memory.set_pid(1234);
        memory.mark_ready();
        guard.handoff();

        let snapshot = reservations.snapshot();
        let entry = &snapshot.entries[&id];
        assert_eq!(entry.pid, Some(1234));
        assert!(entry.ready);
        assert!(!entry.pending);
    }

    #[test]
    fn finish_removes_record_once_and_later_updates_are_no_ops() {
        let reservations = Arc::new(SpawnReservations::default());
        let guard = reservations.reserve("m:p".to_string(), (400, 800), None);
        let memory = guard.memory();
        let revision = reservations.snapshot().revision;

        memory.finish();
        let finished_revision = reservations.snapshot().revision;
        assert!(reservations.snapshot().entries.is_empty());
        assert_ne!(finished_revision, revision);

        memory.finish();
        memory.set_pid(1234);
        memory.mark_ready();
        assert_eq!(reservations.snapshot().revision, finished_revision);
        assert!(reservations.snapshot().entries.is_empty());
    }

    #[test]
    fn abandonment_drops_memory_before_callback() {
        let reservations = Arc::new(SpawnReservations::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let observed_empty = Arc::new(AtomicUsize::new(0));
        let callback_reservations = reservations.clone();
        let callback_calls = calls.clone();
        let callback_observed_empty = observed_empty.clone();
        let guard = reservations.reserve(
            "m:p".to_string(),
            (400, 800),
            Some(Box::new(move || {
                callback_calls.fetch_add(1, Ordering::SeqCst);
                if callback_reservations.snapshot().entries.is_empty() {
                    callback_observed_empty.fetch_add(1, Ordering::SeqCst);
                }
            })),
        );

        drop(guard);

        assert_eq!(calls.load(Ordering::SeqCst), 1);
        assert_eq!(observed_empty.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn handoff_does_not_run_abandonment_callback() {
        let reservations = Arc::new(SpawnReservations::default());
        let calls = Arc::new(AtomicUsize::new(0));
        let callback_calls = calls.clone();
        let mut guard = reservations.reserve(
            "m:p".to_string(),
            (400, 800),
            Some(Box::new(move || {
                callback_calls.fetch_add(1, Ordering::SeqCst);
            })),
        );

        guard.handoff();
        drop(guard);
        assert_eq!(calls.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn counts_are_per_key_and_only_pending() {
        let reservations = Arc::new(SpawnReservations::default());
        let mut g1 = reservations.reserve("m:a".to_string(), (1, 1), None);
        let g2 = reservations.reserve("m:a".to_string(), (1, 1), None);
        let g3 = reservations.reserve("m:b".to_string(), (1, 1), None);

        g1.handoff();
        assert_eq!(reservations.profile_count("m:a"), 1);
        assert_eq!(reservations.profile_count("m:b"), 1);
        assert_eq!(reservations.node_total(), 2);

        drop(g1);
        drop(g2);
        drop(g3);
        assert!(reservations.snapshot().entries.is_empty());
    }

    #[test]
    fn revision_conditional_commit_rejects_stale_snapshot() {
        let reservations = Arc::new(SpawnReservations::default());
        let initial_revision = reservations.snapshot().revision;
        assert_eq!(
            reservations.with_revision(initial_revision, || 42),
            Some(42)
        );

        let guard = reservations.reserve("m:p".to_string(), (1, 1), None);
        assert_eq!(reservations.with_revision(initial_revision, || 42), None);
        let current_revision = reservations.snapshot().revision;
        assert_eq!(
            reservations.with_revision(current_revision, || 42),
            Some(42)
        );

        drop(guard);
        assert_ne!(reservations.snapshot().revision, current_revision);
    }
}
