//! Phase-3 COMPARE protocol acceptance tests — the no-op fast-path + catalog digest.
//!
//! Drives the public `Vault` API over the in-memory `Vec<u8>` seam — no real
//! on-disk vault, no iroh. Mirrors `tests/sync.rs`: `mod common; use common::*;`.
//!
//! Covers the digest fast-path (`SyncRequest → InSync | DigestMismatch →
//! SyncExchange → SyncResponse`) — the no-op O(1) steady state and its miss path —
//! plus `catalog_digest()`, the whole-vault fingerprint two replicas compare in one
//! round-trip to tell whether they are fully in sync (Prune-0).

use vault_sync::{FileSystem, SyncMessage};

mod common;
use common::*;

/// AC-§6 (the §6.2 no-op-cheapness half): a fully-synced pair terminates the sync in
/// O(1) wire payload via the catalog-digest fast-path. The opener carries ONLY the
/// digest (zero per-document VVs); on a digest match the peer replies `InSync` and the
/// exchange ends with zero document content transferred. A real (digest-miss) sync
/// pays one extra round-trip — `SyncRequest → DigestMismatch → SyncExchange →
/// SyncResponse` — but still converges with minimal deltas in BOTH directions.
mod ac_6_no_op_cheap {
    use super::*;

    /// The no-op core: a converged pair syncs in two messages — `SyncRequest` then a
    /// terminal `InSync` reply — and ZERO document content crosses the wire. This is the
    /// §6.2 O(1) no-op guarantee (the common steady state).
    #[tokio::test]
    async fn converged_pair_syncs_in_one_in_sync_reply() {
        let (a, b, fs_a, fs_b) = two_vaults().await;

        write_and_index(&a, &fs_a, "notes/alpha.md", "# Alpha\n\nbody\n").await;
        write_and_index(&b, &fs_b, "notes/beta.md", "# Beta\n\nbody\n").await;
        sync_both_ways(&a, &b).await;
        assert_converged(&a, &fs_a, &b, &fs_b).await;

        // Drive the handshake from the opener and inspect the FIRST reply.
        let mut counter = ByteCounter::new();
        let request = a.prepare_request().await.unwrap();
        counter
            .document_content_per_message
            .push(document_content_bytes(&decode(&request)));
        let outcome = b.process_message(&request).await.unwrap();

        let reply = outcome
            .reply
            .expect("a converged peer still replies (InSync)");
        assert!(
            matches!(decode(&reply), SyncMessage::InSync),
            "a converged pair's first reply is InSync, got {:?}",
            decode(&reply)
        );
        counter
            .document_content_per_message
            .push(document_content_bytes(&decode(&reply)));

        // InSync terminates the exchange: feeding it back to A yields no further reply.
        let after = a.process_message(&reply).await.unwrap();
        assert!(
            after.reply.is_none(),
            "InSync terminates the exchange (no reply)"
        );
        assert!(after.modified.is_empty(), "a no-op sync modifies nothing");

        assert_eq!(
            counter.total_document_content_bytes(),
            0,
            "a converged (no-op) sync transfers zero document content"
        );
    }

    /// The O(1)-payload requirement: the opener enumerates ZERO per-document version
    /// vectors — the digest stands in for them. (A digest-only opener by construction;
    /// without this the steady-state no-op would cost O(document-count).)
    #[tokio::test]
    async fn no_op_opener_enumerates_zero_versions() {
        let (a, _b, fs_a, _fs_b) = two_vaults().await;

        // Even with documents present, the opener carries no per-document VVs.
        for i in 0..5 {
            write_and_index(&a, &fs_a, &format!("doc-{i}.md"), &format!("body {i}\n")).await;
        }

        let request = a.prepare_request().await.unwrap();
        match decode(&request) {
            SyncMessage::SyncRequest {
                document_versions, ..
            } => assert!(
                document_versions.is_empty(),
                "the O(1) opener carries zero per-document VVs (the digest stands in), got {}",
                document_versions.len()
            ),
            other => panic!("prepare_request produced a non-SyncRequest: {other:?}"),
        }
    }

    /// THE load-bearing regression: a digest MISS with divergence in BOTH directions
    /// still converges with minimal deltas. A edits doc X (WeAhead), B edits doc Y
    /// (TheyAhead), and each side creates a brand-new exclusive doc — so the
    /// four-message miss path (`SyncRequest → DigestMismatch → SyncExchange →
    /// SyncResponse`) must flow deltas the RIGHT way in BOTH directions. Proves the
    /// digest layer never traps a real change behind a false "in sync."
    #[tokio::test]
    async fn digest_miss_still_converges() {
        let (a, b, fs_a, fs_b) = two_vaults().await;

        // A shared base for the bidirectional edits, including a doc both sides edit
        // concurrently (the case that most stresses the miss path: a single doc's
        // content must round-trip in BOTH directions).
        write_and_index(&a, &fs_a, "x.md", "base x\n").await;
        write_and_index(&a, &fs_a, "y.md", "base y\n").await;
        write_and_index(&a, &fs_a, "shared.md", "base shared\n").await;
        sync_both_ways(&a, &b).await;
        assert_converged(&a, &fs_a, &b, &fs_b).await;

        // Diverge in both directions: A edits x (WeAhead), B edits y (TheyAhead), BOTH
        // edit shared.md offline (concurrent), plus a brand-new exclusive doc each side.
        write_and_index(&a, &fs_a, "x.md", "base x\nA edited x\n").await;
        write_and_index(&b, &fs_b, "y.md", "base y\nB edited y\n").await;
        write_and_index(&a, &fs_a, "shared.md", "base shared\nA's line\n").await;
        write_and_index(&b, &fs_b, "shared.md", "base shared\nB's line\n").await;
        write_and_index(&a, &fs_a, "new-on-a.md", "A exclusive\n").await;
        write_and_index(&b, &fs_b, "new-on-b.md", "B exclusive\n").await;

        // Precondition: the digests differ (a real miss), so the fast path correctly
        // falls through to the four-message exchange.
        assert_ne!(
            a.catalog_digest(),
            b.catalog_digest(),
            "precondition: divergence ⇒ digests differ ⇒ a real miss"
        );

        // Drive ONE direction (A→B). B now holds both edits to shared.md; A applied B's
        // Index meta for shared.md (riding the forced full snapshot) but NOT B's content
        // (the exclude path withheld it). The digests MUST NOT falsely match here — A
        // still lacks B's line. This is the direct guard on content_version being a
        // per-replica-LOCAL value (Resolution #2): because the digest reads A's local
        // table — not the merged Index meta — B's value can't poison A's digest, so the
        // reverse leg correctly still sees a difference instead of short-circuiting on a
        // false InSync and trapping the divergence.
        full_sync(&a, &b).await;
        assert_ne!(
            a.catalog_digest(),
            b.catalog_digest(),
            "no false match: one direction synced, but A still lacks B's content for shared.md"
        );

        // The reverse leg then converges (it does NOT short-circuit, because the digests
        // correctly still differ).
        full_sync(&b, &a).await;
        assert_converged(&a, &fs_a, &b, &fs_b).await;

        // Every divergence direction landed on both replicas.
        let merged_x = read_md_str(&fs_a, "x.md").await;
        assert!(
            merged_x.contains("A edited x"),
            "A's edit to x survived: {merged_x:?}"
        );
        let merged_y = read_md_str(&fs_a, "y.md").await;
        assert!(
            merged_y.contains("B edited y"),
            "B's edit to y reached A: {merged_y:?}"
        );
        // The concurrently-edited shared doc has BOTH lines on BOTH replicas (the
        // single-doc round-trip the miss path must complete in both directions).
        let shared_a = read_md_str(&fs_a, "shared.md").await;
        assert!(
            shared_a.contains("A's line") && shared_a.contains("B's line"),
            "shared.md merged both edits on A: {shared_a:?}"
        );
        assert!(
            fs_a.exists("new-on-b.md").await.unwrap(),
            "B's exclusive doc reached A"
        );
        assert!(
            fs_b.exists("new-on-a.md").await.unwrap(),
            "A's exclusive doc reached B"
        );

        // And after converging, the digests agree (the fast path now correctly matches).
        assert_eq!(
            a.catalog_digest(),
            b.catalog_digest(),
            "after converging, the digests match (no residual divergence)"
        );
    }

    /// First contact: A has documents, B is empty (so the digests differ) → the miss
    /// path materializes everything on B. The bootstrap case the digest fast-path must
    /// fall through to a full sync for.
    #[tokio::test]
    async fn first_contact_falls_through_to_full_sync() {
        let (a, b, fs_a, fs_b) = two_vaults().await;

        write_and_index(&a, &fs_a, "notes/one.md", "# One\n\nbody\n").await;
        write_and_index(&a, &fs_a, "notes/two.md", "# Two\n\nbody\n").await;
        // B is empty — first contact, digests differ.
        assert_ne!(
            a.catalog_digest(),
            b.catalog_digest(),
            "precondition: a populated vault and an empty one have different digests"
        );

        sync_both_ways(&a, &b).await;
        assert_converged(&a, &fs_a, &b, &fs_b).await;
        assert!(
            fs_b.exists("notes/one.md").await.unwrap(),
            "first contact materialized one.md on B"
        );
        assert!(
            fs_b.exists("notes/two.md").await.unwrap(),
            "first contact materialized two.md on B"
        );
    }
}

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
