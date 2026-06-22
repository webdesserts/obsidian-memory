//! Phase-3 COMPARE protocol acceptance tests (the efficiency layer).
//!
//! Drives the public `Vault` API over the in-memory `Vec<u8>` seam — no real
//! on-disk vault, no iroh. Mirrors `tests/sync.rs`: `mod common; use common::*;`.
//!
//! Chunk A covers `catalog_digest()` — the whole-vault fingerprint two replicas
//! compare in one round-trip to tell whether they are fully in sync (Prune-0).

use vault_sync::FileSystem;

mod common;
use common::*;

/// AC-§6 (Prune-0): the catalog digest is the cheap discriminator for a no-op sync.
/// Two replicas with identical merged state produce byte-equal digests; any change
/// — content edit, move, delete — shifts the digest so the fast path correctly
/// misses and the handshake falls through to a full compare.
mod ac_catalog_digest {
    use super::*;

    /// The baseline: two replicas converged to the same state agree on the digest.
    /// This is what makes "digests equal ⇒ in sync" sound.
    #[tokio::test]
    async fn identical_vaults_have_equal_digests() {
        let (a, b, fs_a, fs_b) = two_vaults().await;

        write_and_index(&a, &fs_a, "notes/alpha.md", "# Alpha\n\nbody\n").await;
        write_and_index(&b, &fs_b, "notes/alpha.md", "# Alpha\n\nbody\n").await;
        full_sync(&a, &b).await;
        full_sync(&b, &a).await;

        assert_converged(&a, &fs_a, &b, &fs_b).await;
        assert_eq!(
            a.catalog_digest(),
            b.catalog_digest(),
            "two replicas at identical merged state must produce byte-equal digests"
        );
    }

    /// The determinism tripwire: two replicas that authored the SAME documents in
    /// the OPPOSITE order still agree on the digest after converging. Pins the
    /// sort-by-uuid — a non-sorted concatenation would diverge here. (The digest
    /// analogue of AC-INV-4-DET.)
    #[tokio::test]
    async fn digest_is_order_independent() {
        let (a, b, fs_a, fs_b) = two_vaults().await;

        // A authors one.md then two.md; B authors two.md then one.md.
        write_and_index(&a, &fs_a, "one.md", "first doc\n").await;
        write_and_index(&a, &fs_a, "two.md", "second doc\n").await;
        write_and_index(&b, &fs_b, "two.md", "second doc\n").await;
        write_and_index(&b, &fs_b, "one.md", "first doc\n").await;

        full_sync(&a, &b).await;
        full_sync(&b, &a).await;

        assert_converged(&a, &fs_a, &b, &fs_b).await;
        assert_eq!(
            a.catalog_digest(),
            b.catalog_digest(),
            "the digest must be independent of the order documents were authored in (sort-by-uuid)"
        );
    }

    /// An edit on A shifts A's digest — away from B's AND away from A's own pre-edit
    /// digest. Proves `content_version` actually feeds the digest (an edit that
    /// didn't change the digest would let the fast path falsely report "in sync").
    #[tokio::test]
    async fn an_edit_changes_the_digest() {
        let (a, b, fs_a, _fs_b) = two_vaults().await;

        write_and_index(&a, &fs_a, "doc.md", "original body\n").await;
        full_sync(&a, &b).await;
        let before = a.catalog_digest();
        assert_eq!(
            before,
            b.catalog_digest(),
            "precondition: converged + equal"
        );

        // Edit the doc on A only.
        write_and_index(&a, &fs_a, "doc.md", "edited body — different\n").await;

        assert_ne!(
            a.catalog_digest(),
            before,
            "an edit must change A's own digest (content_version feeds the digest)"
        );
        assert_ne!(
            a.catalog_digest(),
            b.catalog_digest(),
            "after an edit on A only, A and B must have different digests"
        );
    }

    /// A PURE MOVE on A shifts A's digest even though no `content_version` changed —
    /// because the move bumps the Index VV. The subtle one: without the Index-VV term
    /// the move would be invisible to the digest and a peer would never learn it via
    /// the fast path. Pins the Index-VV-in-digest requirement.
    #[tokio::test]
    async fn a_pure_move_changes_the_digest() {
        let (a, b, fs_a, _fs_b) = two_vaults().await;

        write_and_index(&a, &fs_a, "old/name.md", "stable content\n").await;
        full_sync(&a, &b).await;
        let before = a.catalog_digest();
        assert_eq!(
            before,
            b.catalog_digest(),
            "precondition: converged + equal"
        );

        // Pure structural move — content is path-independent, so no content_version
        // changes; only the Index VV advances.
        move_file(&a, &fs_a, "old/name.md", "new/name.md").await;

        assert_ne!(
            a.catalog_digest(),
            before,
            "a pure move must change the digest via the Index VV (no content_version changed)"
        );
        assert_ne!(
            a.catalog_digest(),
            b.catalog_digest(),
            "after a move on A only, A and B must have different digests so B learns the move"
        );
    }

    /// Deleting a doc shifts the digest, and re-converging brings the digests back
    /// together — a tombstoned node contributes nothing, so the post-delete merged
    /// state on both replicas hashes equal. Kept behavioral (no white-box reach-in):
    /// delete on A → digests differ → sync both ways → digests equal again.
    #[tokio::test]
    async fn deleting_a_doc_changes_the_digest() {
        let (a, b, fs_a, fs_b) = two_vaults().await;

        write_and_index(&a, &fs_a, "keep.md", "keep this\n").await;
        write_and_index(&a, &fs_a, "doomed.md", "delete me\n").await;
        full_sync(&a, &b).await;
        assert_eq!(
            a.catalog_digest(),
            b.catalog_digest(),
            "precondition: both docs converged + equal digests"
        );

        // Delete one doc on A (Index tombstone + fs cleanup).
        a.index().delete_node("doomed.md").unwrap();
        a.save_index().await.unwrap();
        fs_a.delete("doomed.md").await.unwrap();

        assert_ne!(
            a.catalog_digest(),
            b.catalog_digest(),
            "after a delete on A only, A's digest must differ from B's"
        );

        // Reconcile the delete to B; the tombstone contributes nothing, so both
        // replicas hash the same one-doc merged state.
        sync_both_ways(&a, &b).await;
        assert_converged(&a, &fs_a, &b, &fs_b).await;
        assert_eq!(
            a.catalog_digest(),
            b.catalog_digest(),
            "after the delete propagates, the digests re-converge (tombstone contributes nothing)"
        );
    }

    /// Two fresh empty replicas agree on the digest — the no-op opener for an empty
    /// mesh (digest over just the empty Index VV).
    #[tokio::test]
    async fn empty_vaults_have_equal_digests() {
        let (a, _b, _fs_a, _fs_b) = two_vaults().await;
        let (b, _, _, _) = two_vaults().await;

        assert_eq!(
            a.catalog_digest(),
            b.catalog_digest(),
            "two fresh empty vaults must produce equal digests"
        );
    }
}
