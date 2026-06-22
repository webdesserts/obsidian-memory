//! Native-move coalescing buffer (P4f-1 — files only).
//!
//! macOS FSEvents (and the watcher built on it) give no rename linkage: a native
//! file move/rename surfaces as a `Deleted(old)` plus a `Modified(new)`, unlinked
//! and in EITHER order (see `watcher.rs`). Dispatched naively that is a tombstone
//! plus a fresh-UUID create — the document's lineage (its UUID + CRDT history) is
//! lost, and a peer re-downloads the full content under a new identity.
//!
//! This buffer re-stitches the pair. It holds not-yet-tracked creates and deletes
//! for a short window (`MOVE_WINDOW`), keyed by the document's portable
//! [`content_hash`](vault_sync::content_hash). When the second half of a pair
//! arrives within the window — in either order — the two collapse into ONE
//! same-UUID move (`Index::move_node`), which re-parents the existing node and
//! re-transfers zero content (INV-1). On window expiry with no partner, a buffered
//! event commits to its standalone meaning: a lone delete is a real tombstone, a
//! lone create is a real new document (never a silent drop).
//!
//! ## Determinism boundary (why this is safe)
//!
//! Move DETECTION is a local, wall-clock heuristic over ONE machine's event
//! stream — it never enters the convergence/merge math. Whatever crosses the wire
//! is either a `tree.mov` or a (tombstone + new node), both of which the
//! deterministic merge layer resolves to one converged state with content always
//! preserved. Two replicas can detect the same user action via different local
//! heuristics (one coalesces, the other window-misses and sees create+delete) and
//! still converge. So all timing/ordering logic lives here, in the daemon, and no
//! wall-clock value ever reaches a synced op's payload.
//!
//! ## Scope (P4f-1)
//!
//! Files only, in-memory only. No persistence/crash-recovery (P4f-2) and no
//! folder-move recognizer (P4f-3). The buffer is intentionally pure: it owns no
//! vault handle and performs no I/O. The caller (the daemon) computes content
//! hashes against the vault, feeds events in, and executes the [`MoveDecision`]s
//! and [`Expired`] records the buffer returns. That keeps the timing state machine
//! unit-testable in isolation.

use std::collections::{HashMap, VecDeque};
use std::time::{Duration, Instant};

use sync_core::time_scale::scaled;

/// How long a not-yet-tracked create/delete waits for its partner before it
/// commits standalone.
///
/// Must exceed the watcher's 200ms debounce period, or a move's two halves can't
/// both clear the debouncer and land in one window; 500ms gives ~2 debounce
/// periods of slack. The upper bound is the added propagation latency on a REAL
/// delete/create (it can't commit until proven not-a-move) — imperceptible for
/// notes and within the eventually-consistent envelope. Wrapped in
/// [`scaled`] so the time-dependent tests shrink it consistently; at the
/// production scale of 1.0 it is exactly 500ms.
const MOVE_WINDOW: Duration = Duration::from_millis(500);

/// A delete event awaiting a possible partner create. The node is NOT yet
/// tombstoned — the buffer holds the event so a later [`MoveDecision::Move`] can
/// re-parent the still-live node via `move_node`.
#[derive(Debug, Clone)]
struct PendingDelete {
    old_path: String,
    deadline: Instant,
}

/// A create event (a `Modified` at a not-yet-tracked path) awaiting a possible
/// partner delete. No node has been minted for it — the buffer holds the event so
/// a later [`MoveDecision::Move`] re-parents the deleted node onto this path
/// instead of minting a fresh UUID. The content is on disk (re-hashable), so we
/// hold no bytes.
#[derive(Debug, Clone)]
struct PendingCreate {
    new_path: String,
    deadline: Instant,
}

/// What the daemon should do in response to feeding an event into the coalescer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MoveDecision {
    /// A create⇄delete pair matched: execute ONE same-UUID move from `old_path`
    /// to `new_path` (`Index::move_node`). Re-parents the existing node, preserves
    /// its UUID + content, re-transfers nothing.
    Move { old_path: String, new_path: String },
    /// No partner yet — the event was buffered. Nothing to do until its partner
    /// arrives or its window expires (caught by the sweep).
    Buffered,
}

/// A buffered event whose window expired with no partner — it must now commit to
/// its standalone meaning.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Expired {
    /// A lone delete past its window — a real deletion. Dispatch to the standalone
    /// delete sink (`on_file_deleted` → tombstone + broadcast).
    StandaloneDelete { path: String },
    /// A lone create past its window — a real new document. Dispatch to the
    /// standalone create sink (`on_file_modified` → mint fresh UUID + broadcast).
    StandaloneCreate { path: String },
}

/// Bounded, in-memory create⇄delete pairing buffer (P4f-1).
///
/// Keyed by content hash so the two halves of a move pair regardless of arrival
/// order. Each hash bucket is a FIFO queue so two identical-content files in one
/// window pair 1:1 without losing either (the pathological collision case is
/// hardened further in P4f-3; FIFO is the safe first cut).
pub(crate) struct MoveCoalescer {
    pending_deletes: HashMap<[u8; 32], VecDeque<PendingDelete>>,
    pending_creates: HashMap<[u8; 32], VecDeque<PendingCreate>>,
}

impl MoveCoalescer {
    pub(crate) fn new() -> Self {
        Self {
            pending_deletes: HashMap::new(),
            pending_creates: HashMap::new(),
        }
    }

    /// The sweep cadence — a fraction of the window so an expiry is caught
    /// promptly rather than up to a full window late. Wrapped in [`scaled`] to
    /// track the test time-scale alongside [`MOVE_WINDOW`].
    pub(crate) fn sweep_interval() -> Duration {
        scaled(MOVE_WINDOW / 2)
    }

    /// Whether a delete for `path` is currently buffered.
    ///
    /// The router consults this so a `Modified` at a path with a still-live node
    /// but a buffered delete (a re-create at a just-deleted path, OQ-D) is treated
    /// as a create-candidate rather than the edit fast-path — the node is only
    /// "live" because the buffer is deliberately holding its deletion.
    pub(crate) fn has_pending_delete(&self, path: &str) -> bool {
        self.pending_deletes
            .values()
            .any(|q| q.iter().any(|d| d.old_path == path))
    }

    /// Feed a delete event (the old half of a possible move).
    ///
    /// `hash` is the content hash of the doc at `path`, computed by the caller
    /// from its still-present `ContentDoc` BEFORE any tombstone. If a buffered
    /// create already matches that hash, the pair fires now (the create-arrived-
    /// first case); otherwise the delete is buffered for one window.
    pub(crate) fn on_delete(&mut self, hash: [u8; 32], path: &str) -> MoveDecision {
        if let Some(create) = self.pop_pending_create(&hash) {
            return MoveDecision::Move {
                old_path: path.to_string(),
                new_path: create.new_path,
            };
        }
        self.pending_deletes
            .entry(hash)
            .or_default()
            .push_back(PendingDelete {
                old_path: path.to_string(),
                deadline: Instant::now() + scaled(MOVE_WINDOW),
            });
        MoveDecision::Buffered
    }

    /// Feed a create event (the new half of a possible move).
    ///
    /// `hash` is the content hash of the new on-disk `.md`, computed by the caller
    /// in the same hash domain as the delete side. If a buffered delete already
    /// matches, the pair fires now (the common delete-arrived-first case);
    /// otherwise the create is buffered for one window.
    pub(crate) fn on_create(&mut self, hash: [u8; 32], path: &str) -> MoveDecision {
        if let Some(delete) = self.pop_pending_delete(&hash) {
            return MoveDecision::Move {
                old_path: delete.old_path,
                new_path: path.to_string(),
            };
        }
        self.pending_creates
            .entry(hash)
            .or_default()
            .push_back(PendingCreate {
                new_path: path.to_string(),
                deadline: Instant::now() + scaled(MOVE_WINDOW),
            });
        MoveDecision::Buffered
    }

    /// Drain every buffered event whose deadline has passed, returning each one's
    /// standalone meaning. Called from the daemon's sweep-timer arm.
    pub(crate) fn sweep(&mut self) -> Vec<Expired> {
        let now = Instant::now();
        let mut expired = Vec::new();

        retain_expired(&mut self.pending_deletes, now, |d| {
            expired.push(Expired::StandaloneDelete {
                path: d.old_path.clone(),
            });
        });
        retain_expired(&mut self.pending_creates, now, |c| {
            expired.push(Expired::StandaloneCreate {
                path: c.new_path.clone(),
            });
        });

        expired
    }

    /// Pop the oldest buffered create for `hash`, removing the bucket if it empties.
    fn pop_pending_create(&mut self, hash: &[u8; 32]) -> Option<PendingCreate> {
        let queue = self.pending_creates.get_mut(hash)?;
        let create = queue.pop_front();
        if queue.is_empty() {
            self.pending_creates.remove(hash);
        }
        create
    }

    /// Pop the oldest buffered delete for `hash`, removing the bucket if it empties.
    fn pop_pending_delete(&mut self, hash: &[u8; 32]) -> Option<PendingDelete> {
        let queue = self.pending_deletes.get_mut(hash)?;
        let delete = queue.pop_front();
        if queue.is_empty() {
            self.pending_deletes.remove(hash);
        }
        delete
    }
}

/// Remove every record in `map` whose deadline has passed, invoking `on_expire`
/// for each, and drop any bucket that empties.
fn retain_expired<T, F>(map: &mut HashMap<[u8; 32], VecDeque<T>>, now: Instant, mut on_expire: F)
where
    T: HasDeadline,
    F: FnMut(&T),
{
    map.retain(|_, queue| {
        queue.retain(|record| {
            if record.deadline() <= now {
                on_expire(record);
                false
            } else {
                true
            }
        });
        !queue.is_empty()
    });
}

/// Shared deadline accessor so the sweep can treat pending creates and deletes
/// uniformly.
trait HasDeadline {
    fn deadline(&self) -> Instant;
}

impl HasDeadline for PendingDelete {
    fn deadline(&self) -> Instant {
        self.deadline
    }
}

impl HasDeadline for PendingCreate {
    fn deadline(&self) -> Instant {
        self.deadline
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const HASH_A: [u8; 32] = [0xAA; 32];
    const HASH_B: [u8; 32] = [0xBB; 32];

    /// The common case: a delete buffers, then a content-matching create pairs
    /// with it into a single move (delete-arrived-first).
    #[test]
    fn delete_then_matching_create_emits_move() {
        let mut c = MoveCoalescer::new();
        assert_eq!(c.on_delete(HASH_A, "old.md"), MoveDecision::Buffered);
        assert_eq!(
            c.on_create(HASH_A, "new.md"),
            MoveDecision::Move {
                old_path: "old.md".to_string(),
                new_path: "new.md".to_string(),
            }
        );
        // Both halves consumed — nothing left to sweep.
        assert!(c.sweep().is_empty());
    }

    /// The symmetry the old design couldn't express: the create half can arrive
    /// first and still pair into the same move.
    #[test]
    fn create_then_matching_delete_emits_move() {
        let mut c = MoveCoalescer::new();
        assert_eq!(c.on_create(HASH_A, "new.md"), MoveDecision::Buffered);
        assert_eq!(
            c.on_delete(HASH_A, "old.md"),
            MoveDecision::Move {
                old_path: "old.md".to_string(),
                new_path: "new.md".to_string(),
            }
        );
        assert!(c.sweep().is_empty());
    }

    /// A delete with no content-matching create expires to a real tombstone.
    #[test]
    fn lone_delete_expires_to_standalone_delete() {
        let mut c = MoveCoalescer::new();
        c.on_delete(HASH_A, "gone.md");
        c.expire_all_for_test();
        assert_eq!(
            c.sweep(),
            vec![Expired::StandaloneDelete {
                path: "gone.md".to_string()
            }]
        );
    }

    /// A create with no content-matching delete expires to a real new document.
    #[test]
    fn lone_create_expires_to_standalone_create() {
        let mut c = MoveCoalescer::new();
        c.on_create(HASH_A, "fresh.md");
        c.expire_all_for_test();
        assert_eq!(
            c.sweep(),
            vec![Expired::StandaloneCreate {
                path: "fresh.md".to_string()
            }]
        );
    }

    /// A same-content create arriving AFTER the delete's window expired finds
    /// nothing pending — it is a NEW document, not an adoption of the old lineage.
    /// (The EC-8 ancient-coincidence non-adopt, at the buffer level.)
    #[test]
    fn matching_create_after_window_does_not_pair() {
        let mut c = MoveCoalescer::new();
        c.on_delete(HASH_A, "old.md");
        // The delete's window elapses and it commits standalone.
        c.expire_all_for_test();
        assert_eq!(
            c.sweep(),
            vec![Expired::StandaloneDelete {
                path: "old.md".to_string()
            }]
        );
        // A later same-content create has no pending delete to adopt onto.
        assert_eq!(c.on_create(HASH_A, "other.md"), MoveDecision::Buffered);
    }

    /// Non-matching content does not pair: a delete and a create with different
    /// hashes both sit buffered until their windows expire standalone.
    #[test]
    fn different_content_does_not_pair() {
        let mut c = MoveCoalescer::new();
        assert_eq!(c.on_delete(HASH_A, "a.md"), MoveDecision::Buffered);
        assert_eq!(c.on_create(HASH_B, "b.md"), MoveDecision::Buffered);
        c.expire_all_for_test();
        let mut expired = c.sweep();
        expired.sort_by_key(|e| format!("{e:?}"));
        assert_eq!(
            expired,
            vec![
                Expired::StandaloneCreate {
                    path: "b.md".to_string()
                },
                Expired::StandaloneDelete {
                    path: "a.md".to_string()
                },
            ]
        );
    }

    /// `has_pending_delete` reports a buffered delete by path — the signal the
    /// router uses to route a re-create-at-a-just-deleted-path (OQ-D) to the
    /// create branch instead of the edit fast-path.
    #[test]
    fn has_pending_delete_tracks_buffered_deletes() {
        let mut c = MoveCoalescer::new();
        assert!(!c.has_pending_delete("p.md"));
        c.on_delete(HASH_A, "p.md");
        assert!(c.has_pending_delete("p.md"));
        // Pairing it consumes the pending delete.
        c.on_create(HASH_A, "p.md");
        assert!(!c.has_pending_delete("p.md"));
    }

    impl MoveCoalescer {
        /// Force every buffered record's deadline into the past so the next
        /// `sweep()` expires it — lets the unit tests exercise expiry without
        /// sleeping. The integration tests drive the real (scaled) window.
        fn expire_all_for_test(&mut self) {
            let past = Instant::now() - Duration::from_secs(3600);
            for queue in self.pending_deletes.values_mut() {
                for record in queue.iter_mut() {
                    record.deadline = past;
                }
            }
            for queue in self.pending_creates.values_mut() {
                for record in queue.iter_mut() {
                    record.deadline = past;
                }
            }
        }
    }
}
