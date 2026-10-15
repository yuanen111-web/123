use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;

/// Cache-line-aligned wrapper isolating hot seqlock state from waiter-list
/// and telemetry mutations (adapted from `facebookexperimental/reverie`
/// `reverie-rpc-transport/src/guest_log/buffer.rs:38-77` and
/// `reverie-rpc-transport/src/guest_log/buffer/ordered.rs:78-100`, BSD-2-Clause).
#[repr(C, align(64))]
struct CacheAligned<T>(T);

struct SeqlockState {
    generation: AtomicU64,
    in_flight: AtomicU64,
    poisoned: std::sync::atomic::AtomicBool,
}

/// Per-index seqlock-style publication barrier for collection-wide captures.
/// Writers may overlap, but a reader only accepts a capture made while no
/// writer was active and the generation remained unchanged.
pub(crate) struct CollectionPublication {
    seqlock: CacheAligned<SeqlockState>,
    notify: CacheAligned<tokio::sync::Notify>,
    #[cfg(test)]
    reader_admission_attempts: CacheAligned<AtomicU64>,
}

pub(crate) struct CollectionPublicationGuard {
    state: Arc<CollectionPublication>,
    outcome: GuardOutcome,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct CollectionReadToken {
    generation: u64,
}

impl CollectionReadToken {
    pub(crate) fn generation(self) -> u64 {
        self.generation
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum ReadAdmission {
    Admitted(CollectionReadToken),
    WriterActive,
    Poisoned,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum GuardOutcome {
    Pending,
    Committed,
    Cancelled,
}

impl CollectionPublication {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            seqlock: CacheAligned(SeqlockState {
                generation: AtomicU64::new(0),
                in_flight: AtomicU64::new(0),
                poisoned: std::sync::atomic::AtomicBool::new(false),
            }),
            notify: CacheAligned(tokio::sync::Notify::new()),
            #[cfg(test)]
            reader_admission_attempts: CacheAligned(AtomicU64::new(0)),
        })
    }

    pub(crate) fn begin(
        self: &Arc<Self>,
    ) -> Result<CollectionPublicationGuard, CollectionPublicationPoisoned> {
        let seqlock = &self.seqlock.0;
        if seqlock.poisoned.load(Ordering::Acquire) {
            return Err(CollectionPublicationPoisoned);
        }
        seqlock.in_flight.fetch_add(1, Ordering::AcqRel);
        seqlock.generation.fetch_add(1, Ordering::AcqRel);
        // A concurrent failed publisher may poison after our first check.
        // This writer has not mutated yet, so withdraw cleanly and reject it.
        if seqlock.poisoned.load(Ordering::Acquire) {
            seqlock.generation.fetch_add(1, Ordering::Release);
            let previous = seqlock.in_flight.fetch_sub(1, Ordering::AcqRel);
            debug_assert!(previous > 0, "collection publication counter underflow");
            if previous == 1 {
                self.notify.0.notify_waiters();
            }
            return Err(CollectionPublicationPoisoned);
        }
        Ok(CollectionPublicationGuard {
            state: Arc::clone(self),
            outcome: GuardOutcome::Pending,
        })
    }

    pub(crate) fn state(&self) -> (u64, u64, bool) {
        let seqlock = &self.seqlock.0;
        loop {
            let before = seqlock.generation.load(Ordering::Acquire);
            let in_flight = seqlock.in_flight.load(Ordering::Acquire);
            let after = seqlock.generation.load(Ordering::Acquire);
            if before == after {
                return (after, in_flight, seqlock.poisoned.load(Ordering::Acquire));
            }
            std::hint::spin_loop();
        }
    }

    pub(crate) fn notified(&self) -> tokio::sync::futures::Notified<'_> {
        self.notify.0.notified()
    }

    pub(crate) fn try_admit_reader(&self) -> ReadAdmission {
        #[cfg(test)]
        self.reader_admission_attempts
            .0
            .fetch_add(1, Ordering::Release);
        let (generation, in_flight, poisoned) = self.state();
        if poisoned {
            ReadAdmission::Poisoned
        } else if in_flight != 0 {
            ReadAdmission::WriterActive
        } else {
            ReadAdmission::Admitted(CollectionReadToken { generation })
        }
    }

    #[cfg(test)]
    pub(crate) fn reader_admission_attempts(&self) -> u64 {
        self.reader_admission_attempts.0.load(Ordering::Acquire)
    }

    pub(crate) fn validate_reader(&self, token: CollectionReadToken) -> ReadAdmission {
        let (generation, in_flight, poisoned) = self.state();
        if poisoned {
            ReadAdmission::Poisoned
        } else if in_flight != 0 || generation != token.generation {
            ReadAdmission::WriterActive
        } else {
            ReadAdmission::Admitted(token)
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct CollectionPublicationPoisoned;

impl CollectionPublicationGuard {
    pub(crate) fn commit(&mut self) {
        self.outcome = GuardOutcome::Committed;
    }
    pub(crate) fn cancel(&mut self) {
        self.outcome = GuardOutcome::Cancelled;
    }
}

impl Drop for CollectionPublicationGuard {
    fn drop(&mut self) {
        if self.outcome == GuardOutcome::Pending {
            self.state.seqlock.0.poisoned.store(true, Ordering::Release);
        }
        self.state
            .seqlock
            .0
            .generation
            .fetch_add(1, Ordering::Release);
        let previous = self
            .state
            .seqlock
            .0
            .in_flight
            .fetch_sub(1, Ordering::AcqRel);
        debug_assert!(previous > 0, "collection publication counter underflow");
        if previous == 1 {
            self.state.notify.0.notify_waiters();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn seqlock_and_reader_counters_sit_on_distinct_64_byte_cache_lines() {
        let state = CollectionPublication::new();
        let seqlock_line = (&raw const state.seqlock) as usize / 64;
        let notify_line = (&raw const state.notify) as usize / 64;
        let reader_line = (&raw const state.reader_admission_attempts) as usize / 64;
        assert_ne!(seqlock_line, notify_line);
        assert_ne!(seqlock_line, reader_line);
        assert_ne!(notify_line, reader_line);
    }

    #[test]
    fn overlapping_writers_keep_capture_unstable_until_last_drop() {
        let state = CollectionPublication::new();
        let mut first = state.begin().unwrap();
        let mut second = state.begin().unwrap();
        assert_eq!(state.state().1, 2);
        first.cancel();
        drop(first);
        assert_eq!(state.state().1, 1);
        second.commit();
        drop(second);
        assert_eq!(state.state().1, 0);
        assert_eq!(state.state().0, 4);
        assert!(!state.state().2);
    }

    #[test]
    fn uncommitted_drop_poison_is_visible_after_liveness_releases() {
        let state = CollectionPublication::new();
        let result = std::panic::catch_unwind({
            let state = Arc::clone(&state);
            move || {
                let _guard = state.begin().unwrap();
                panic!("injected publication panic");
            }
        });
        assert!(result.is_err());
        assert_eq!(state.state().1, 0);
        assert!(state.state().2);
        assert!(state.begin().is_err());
    }

    #[test]
    fn reader_token_is_valid_only_for_one_stable_generation() {
        let state = CollectionPublication::new();
        let token = match state.try_admit_reader() {
            ReadAdmission::Admitted(token) => token,
            other => panic!("unexpected admission: {other:?}"),
        };
        assert_eq!(state.validate_reader(token), ReadAdmission::Admitted(token));
        let mut writer = state.begin().unwrap();
        assert_eq!(state.validate_reader(token), ReadAdmission::WriterActive);
        writer.commit();
        drop(writer);
        assert_eq!(state.validate_reader(token), ReadAdmission::WriterActive);
    }

    #[test]
    fn admitted_peer_may_commit_after_poison_but_poison_stays_sticky() {
        let state = CollectionPublication::new();
        let failed = state.begin().unwrap();
        let mut peer = state.begin().unwrap();
        drop(failed);
        assert!(state.state().2);
        peer.commit();
        drop(peer);
        assert!(state.state().2);
        assert!(state.begin().is_err());
        assert!(matches!(state.try_admit_reader(), ReadAdmission::Poisoned));
    }
}
