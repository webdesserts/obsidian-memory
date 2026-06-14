//! End-to-end proof that the process-global time-scale shrinks a REAL owned
//! duration — the sync-flag TTL (D1, base 30s in `SyncState`).
//!
//! This test seeds the global `set_time_scale`, so it lives in its OWN
//! integration-test file: each integration test compiles to a separate binary,
//! so the first-wins `OnceLock` is naturally isolated to this process and cannot
//! collide with the parallel unit tests (which use the pure `_with` helpers) or
//! any other integration file.

use std::thread::sleep;
use std::time::Duration;

use sync_core::time_scale::set_time_scale;
use sync_core::vault::SyncState;

/// At scale 0.1 the 30s sync-flag TTL collapses to ~3s, so a flag marked synced
/// is still live well before 3s but has expired by ~3.3s — a window that would
/// keep the flag valid for 30s unscaled. A `false` from `consume_synced` after
/// the short wait therefore proves the scale reached a real production duration.
#[test]
fn time_scale_shrinks_sync_flag_ttl() {
    // Seed before touching any scaled duration. First-wins; this binary's process
    // is dedicated to this test, so the seed is uncontended.
    assert!(
        set_time_scale(0.1),
        "set_time_scale should win on first call in a dedicated test binary"
    );

    let state = SyncState::new();
    state.mark_synced("notes/Example.md");

    // The flag is live immediately after marking — the scale only shrinks the
    // TTL, it does not expire flags early.
    assert!(
        state.is_synced("notes/Example.md"),
        "flag should be live right after mark_synced"
    );

    // Wait just past the scaled TTL (3s) but far below the unscaled 30s.
    sleep(Duration::from_millis(3_300));

    // Expired because the TTL was scaled to ~3s. Unscaled this would still be
    // valid for another ~27s, so a `false` here is the end-to-end proof.
    assert!(
        !state.consume_synced("notes/Example.md"),
        "flag should have expired by 3.3s under a 0.1 time-scale (scaled TTL ~3s)"
    );
}
