//! Phase-1 acceptance ROLL-UP: the cross-chunk acceptance criteria that need the
//! whole stack working together, not a single surface.
//!
//! The per-surface ACs already live with their chunks — identity/merge/Flow-2/delete
//! convergence in `sync.rs` (1f), the four reconcile arms + native-move adopt in
//! `reconcile.rs` (1g). This file adds the FULL-STACK coverage those can't express
//! alone:
//!
//! - **AC-INV-4 order-independence** — the same divergent op set delivered in a
//!   SHUFFLED order converges to the SAME result (CRDT commutativity at the seam).
//! - **AC-INV-6/7 boot order** — an offline local edit AND a queued remote op: boot
//!   documents the local edit into the Index BEFORE the remote op applies, so the
//!   local edit is never clobbered ("commit before pull").
//! - **AC-§5 lossy convergence** — N replicas over a drop/dup/reorder channel still
//!   converge, driven by `pump_lossy_to_quiescence`.
//!
//! Everything runs against `InMemoryFs` via the shared [`common`] harness — no test
//! touches a real vault path.

use std::collections::HashMap;
use std::sync::Arc;

use vault_sync::{FileSystem, InMemoryFs, SyncMessage, Vault};

mod common;
use common::*;

// ===================== AC-INV-1 — zero content on move (via the byte counter) =====================

mod ac_inv_1_zero_content_move_byte_counted {
    use super::*;

    /// A full sync that carries ONLY a move (a `tree.mov`) transfers zero
    /// document-content bytes — measured with the harness's [`ByteCounter`] across the
    /// WHOLE handshake, not a single hand-decoded message. This is the full-stack,
    /// byte-counted form of INV-1: `sync.rs` already byte-counts the individual
    /// exchange/response messages of a move; here the reusable counting wrapper sums
    /// every payload of the handshake, exercising the instrument P3's AC-§6 will reuse
    /// for the compare/no-op-cheapness check.
    #[tokio::test]
    async fn full_sync_after_a_move_transfers_zero_document_content_bytes() {
        let (a, b, fs_a, fs_b) = two_vaults().await;

        // Create a document and converge both replicas on it (this sync DOES carry the
        // document's content — the baseline the move is contrasted against).
        write_and_index(&a, &fs_a, "notes/topic.md", "# Topic\n\nA real body.\n").await;
        let mut warmup = ByteCounter::new();
        warmup.full_sync_counting(&a, &b).await;
        assert!(
            warmup.total_document_content_bytes() > 0,
            "the initial sync transferred the document's content (baseline sanity)"
        );

        // A moves the document — a pure-structural Index op, the content `.loro` is
        // path-independent and untouched.
        move_file(&a, &fs_a, "notes/topic.md", "archive/topic.md").await;

        // The move-only sync transfers ZERO document-content bytes across the entire
        // handshake (the move rides the Index delta).
        let mut counter = ByteCounter::new();
        counter.full_sync_counting(&a, &b).await;
        assert_eq!(
            counter.total_document_content_bytes(),
            0,
            "a move-only sync carries no document-content bytes ({:?})",
            counter.document_content_per_message
        );

        // The move still converged: B's document is at the new path, byte-identical,
        // rendered locally from the path-independent content doc (zero bytes crossed).
        assert!(fs_b.exists("archive/topic.md").await.unwrap());
        assert_eq!(
            read_md(&fs_a, "archive/topic.md").await,
            read_md(&fs_b, "archive/topic.md").await,
            "the moved document is byte-identical on both replicas"
        );
    }
}

// ===================== AC-INV-4 — order-independence (shuffled delivery) =====================

mod ac_inv_4_order_independence {
    use super::*;

    /// The SAME set of divergent operations, delivered to a fresh receiver in two
    /// DIFFERENT orders, converges to byte-identical materialized state and identical
    /// Index version vectors — the order-independence half of INV-4 (CRDT
    /// commutativity), at the full seam.
    ///
    /// We build a fixed op set on a source replica (three independent documents, each
    /// with its own create + edit), capture the per-document deltas and the Index
    /// snapshot, then replay them against two fresh receivers: one in authored order,
    /// one in a deterministically-shuffled order. Both must land in exactly the same
    /// place. This is distinct from `sync.rs`'s basic two-replica convergence (which
    /// fixes the delivery order) and from `sync.rs`'s single-document causal-chain
    /// reordering (INV-10) — here whole independent documents arrive interleaved.
    #[tokio::test]
    async fn same_op_set_in_shuffled_delivery_order_converges_identically() {
        // A source replica authors three independent documents, each edited once, so
        // there are six logically-independent payloads (3 creates + 3 edits) plus the
        // Index that carries all three nodes.
        let fs_src = Arc::new(InMemoryFs::new());
        let src = Vault::init(Arc::clone(&fs_src), author(1)).await.unwrap();
        write_and_index(&src, &fs_src, "alpha.md", "# Alpha\n\nbase\n").await;
        write_and_index(&src, &fs_src, "beta.md", "# Beta\n\nbase\n").await;
        write_and_index(&src, &fs_src, "gamma.md", "# Gamma\n\nbase\n").await;
        write_and_index(&src, &fs_src, "alpha.md", "# Alpha\n\nbase\nedited\n").await;
        write_and_index(&src, &fs_src, "beta.md", "# Beta\n\nbase\nedited\n").await;
        write_and_index(&src, &fs_src, "gamma.md", "# Gamma\n\nbase\nedited\n").await;

        // Capture the full Index snapshot (carries all three nodes) and a self-contained
        // snapshot of each document at its final state.
        let index_snapshot = src.index().export_snapshot().unwrap();
        let mut doc_snapshots: HashMap<vault_sync::DocId, Vec<u8>> = HashMap::new();
        for path in ["alpha.md", "beta.md", "gamma.md"] {
            let uuid = vault_sync::DocId(uuid_at(&src, path));
            let snapshot = src
                .get_document(path)
                .await
                .unwrap()
                .export_snapshot()
                .unwrap();
            doc_snapshots.insert(uuid, snapshot);
        }

        // The independent document payloads, each a lone DocUpdate. The Index snapshot
        // is delivered first to satisfy the Flow-2 gate (the nodes must precede the
        // content), so only the ORDER of the three document deliveries is under test.
        let mut payloads: Vec<SyncMessage> = doc_snapshots
            .iter()
            .map(|(uuid, data)| SyncMessage::DocUpdate {
                uuid: *uuid,
                data: data.clone(),
            })
            .collect();

        // Receiver 1: deliver the documents in their natural (HashMap) iteration order.
        let receiver_natural = deliver_in_order(&index_snapshot, &payloads).await;

        // Receiver 2: deliver the SAME documents in a deterministically-shuffled order.
        let mut shuffled = payloads.clone();
        DeterministicRng::new(0xC0FFEE).shuffle(&mut shuffled);
        // Guard the test's own premise: the shuffle must actually reorder, or this
        // would silently pass without testing order-independence.
        assert_ne!(
            payload_uuids(&payloads),
            payload_uuids(&shuffled),
            "the shuffle must reorder the deliveries (otherwise the test is vacuous)"
        );
        payloads = shuffled;
        let receiver_shuffled = deliver_in_order(&index_snapshot, &payloads).await;

        // Both receivers converged to byte-identical materialized state and identical
        // Index version vectors — order did not matter.
        for path in ["alpha.md", "beta.md", "gamma.md"] {
            let md_natural = read_md(&receiver_natural.1, path).await;
            let md_shuffled = read_md(&receiver_shuffled.1, path).await;
            assert_eq!(
                md_natural, md_shuffled,
                "{path} is byte-identical regardless of delivery order"
            );
            // And matches the source's final materialized content.
            assert_eq!(
                md_natural,
                read_md(&fs_src, path).await,
                "{path} converged to the source's final content"
            );
        }
        assert_eq!(
            receiver_natural.0.index().state_vv(),
            receiver_shuffled.0.index().state_vv(),
            "both receivers reach the same Index version vector regardless of order"
        );
    }

    /// Build a fresh receiver, deliver the Index snapshot (nodes first, Flow-2 gate),
    /// then the document payloads in the given order. Returns the receiver and its fs.
    async fn deliver_in_order(index_snapshot: &[u8], payloads: &[SyncMessage]) -> (V, Fs) {
        let fs = Arc::new(InMemoryFs::new());
        let rx = Vault::init(Arc::clone(&fs), author(2)).await.unwrap();

        let bootstrap = SyncMessage::SyncResponse {
            index_updates: Some(index_snapshot.to_vec()),
            document_updates: HashMap::new(),
        };
        rx.process_message(&bincode::serialize(&bootstrap).unwrap())
            .await
            .unwrap();

        for payload in payloads {
            rx.process_message(&bincode::serialize(payload).unwrap())
                .await
                .unwrap();
        }
        (rx, fs)
    }

    /// The ordered list of UUIDs in a payload sequence (for the shuffle-actually-shuffled
    /// premise guard).
    fn payload_uuids(payloads: &[SyncMessage]) -> Vec<vault_sync::DocId> {
        payloads
            .iter()
            .filter_map(|m| match m {
                SyncMessage::DocUpdate { uuid, .. } => Some(*uuid),
                _ => None,
            })
            .collect()
    }
}

// ===================== AC-INV-6/7 — boot order (document-before-integrate) =====================

mod ac_inv_6_7_boot_order {
    use super::*;

    /// An offline local edit AND a queued remote op converge such that the LOCAL edit
    /// is never clobbered: on boot the library documents the local filesystem state
    /// into the Index BEFORE it integrates the remote delta ("commit before pull",
    /// INV-7), so a remote edit to the same document MERGES with the local one rather
    /// than overwriting it (INV-6 Flow-1-then-Flow-2).
    ///
    /// Scenario: A and B converge on a shared document. While B is OFFLINE (its vault
    /// dropped), the `.md` is edited on B's disk (an offline local edit, e.g. the user
    /// editing in their editor with the daemon down) AND A independently edits the same
    /// document (the queued remote op). B then boots — which runs reconcile, capturing
    /// the offline edit into B's Index — and only then syncs with A. Both edits survive.
    #[tokio::test]
    async fn offline_local_edit_is_documented_before_remote_op_integrates() {
        let (a, b, fs_a, fs_b) = two_vaults().await;

        // A shared base document on both replicas.
        write_and_index(&a, &fs_a, "shared.md", "# Shared\n\nbase line\n").await;
        full_sync(&a, &b).await;

        // B goes OFFLINE: drop the vault, leaving its `.sync/` + `.md` on disk. While
        // offline, the `.md` is edited directly on disk (an editor change with the
        // daemon down) — a local edit that ONLY exists on the filesystem, not yet in
        // B's Index.
        drop(b);
        fs_b.write(
            "shared.md",
            b"# Shared\n\nbase line\nlocal offline addition\n",
        )
        .await
        .unwrap();

        // Meanwhile A edits the SAME document (the queued remote op).
        write_and_index(
            &a,
            &fs_a,
            "shared.md",
            "# Shared\n\nbase line\nremote addition\n",
        )
        .await;

        // B BOOTS: `Vault::load` runs reconcile, which documents the offline `.md` edit
        // into B's Index (Flow-1, fs-as-truth) BEFORE any remote integration. This is
        // the load-bearing step — if reconcile did NOT capture the local edit first, the
        // subsequent remote merge would apply against a CRDT that never saw the offline
        // change and the local edit would be lost.
        let b = Vault::load(Arc::clone(&fs_b), author(2)).await.unwrap();

        // Now B integrates A's edit. Sync both ways to quiescence.
        sync_both_ways(&a, &b).await;
        sync_both_ways(&a, &b).await;

        // BOTH edits survive on both replicas — the offline local edit was NOT clobbered
        // by the remote op, and the two merged.
        let md_a = String::from_utf8(read_md(&fs_a, "shared.md").await).unwrap();
        let md_b = String::from_utf8(read_md(&fs_b, "shared.md").await).unwrap();
        assert_eq!(
            md_a, md_b,
            "both replicas converge to byte-identical content"
        );
        assert!(
            md_a.contains("local offline addition"),
            "B's offline local edit survived the boot+remote integration: {md_a:?}"
        );
        assert!(
            md_a.contains("remote addition"),
            "A's remote edit also landed: {md_a:?}"
        );
    }

    /// A boot-order roll-up across the OTHER reconcile arms at once: a single boot that
    /// must, in one reconcile pass and before any remote integration, (a) ADOPT an
    /// orphaned content doc, (b) INDEX a brand-new offline `.md`, and (c) leave an
    /// already-tracked document untouched — then sync the resulting state to a peer.
    /// This exercises that the four-arm reconcile (1g) composes into a coherent boot
    /// state the seam (1f) then ships.
    #[tokio::test]
    async fn boot_reconciles_mixed_local_state_then_syncs_to_peer() {
        // Build a source replica holding one tracked document and an orphaned content
        // doc (content `.loro` + matching `.md`, no node — a peer's content that landed
        // without its node). The source's disk is the staged pre-boot state.
        let fs = Arc::new(InMemoryFs::new());
        let staged = Vault::init(Arc::clone(&fs), author(1)).await.unwrap();
        write_and_index(&staged, &fs, "tracked.md", "# Tracked\n\nalready indexed\n").await;
        let tracked_uuid = uuid_at(&staged, "tracked.md");

        // An orphan: a content doc on disk with no node, plus its `.md`.
        let orphan = vault_sync::ContentDoc::from_markdown(
            "# Orphan\n\ncontent that arrived without a node\n",
            author(1),
        )
        .unwrap();
        let orphan_uuid = uuid::Uuid::parse_str(&orphan.doc_id().unwrap()).unwrap();
        fs.atomic_write(
            &vault_sync::content_doc_path(&orphan_uuid),
            &orphan.export_snapshot().unwrap(),
        )
        .await
        .unwrap();
        fs.write("inbox/orphan.md", orphan.to_markdown().as_bytes())
            .await
            .unwrap();

        // A brand-new offline `.md` (no node, no content doc).
        fs.write("notes/fresh.md", b"# Fresh\n\ncreated while offline\n")
            .await
            .unwrap();

        // BOOT: one reconcile pass adopts the orphan, indexes the fresh file, and leaves
        // the tracked doc as-is — all before remote integration.
        let booted = reload(staged, &fs).await;

        // The orphan adopted with its lineage UUID preserved.
        assert_eq!(
            uuid_at(&booted, "inbox/orphan.md"),
            orphan_uuid,
            "boot adopted the orphan at its `.md` path, preserving lineage"
        );
        // The fresh file was indexed.
        assert!(
            booted.index().node_for_path("notes/fresh.md").is_some(),
            "boot indexed the brand-new offline `.md`"
        );
        // The tracked document kept its identity.
        assert_eq!(
            uuid_at(&booted, "tracked.md"),
            tracked_uuid,
            "the already-tracked document is untouched by reconcile"
        );

        // The reconciled boot state ships cleanly to a fresh peer: a full sync delivers
        // all three documents.
        let fs_peer = Arc::new(InMemoryFs::new());
        let peer = Vault::init(Arc::clone(&fs_peer), author(2)).await.unwrap();
        full_sync(&booted, &peer).await;

        let mut peer_files = peer.list_files().await.unwrap();
        peer_files.sort();
        assert_eq!(
            peer_files,
            vec![
                "inbox/orphan.md".to_string(),
                "notes/fresh.md".to_string(),
                "tracked.md".to_string(),
            ],
            "the peer received every document the boot reconcile produced"
        );
        for path in &peer_files {
            assert_eq!(
                read_md(&fs, path).await,
                read_md(&fs_peer, path).await,
                "{path} is byte-identical on the peer after syncing the reconciled state"
            );
        }
    }
}

// ===================== AC-§5 — lossy convergence via pump_to_quiescence =====================

mod ac_section_5_lossy_convergence {
    use super::*;

    /// Three replicas, each holding an independent document, converge over a LOSSY
    /// channel (random drop/dup/reorder via `pump_lossy_to_quiescence`) to the same
    /// live set and byte-identical content — the §5 seam contract's tolerance of an
    /// unreliable transport, at full N-replica scale.
    ///
    /// This is the full-stack counterpart to `sync.rs`'s two-replica scripted lossy
    /// test: here the loss pattern is randomized (seeded for reproducibility) and the
    /// quiescence driver re-derives convergence across an arbitrary number of faulty
    /// rounds, exactly as a daemon would over a flaky network.
    #[tokio::test]
    async fn three_replicas_converge_over_a_lossy_channel() {
        let fs_a = Arc::new(InMemoryFs::new());
        let fs_b = Arc::new(InMemoryFs::new());
        let fs_c = Arc::new(InMemoryFs::new());
        let a = Vault::init(Arc::clone(&fs_a), author(1)).await.unwrap();
        let b = Vault::init(Arc::clone(&fs_b), author(2)).await.unwrap();
        let c = Vault::init(Arc::clone(&fs_c), author(3)).await.unwrap();

        // Each replica authors an independent document, plus a divergent edit, so there
        // is real cross-replica state to reconcile through the lossy channel.
        write_and_index(&a, &fs_a, "from-a.md", "# A\n\nauthored on A\n").await;
        write_and_index(&b, &fs_b, "from-b.md", "# B\n\nauthored on B\n").await;
        write_and_index(&c, &fs_c, "from-c.md", "# C\n\nauthored on C\n").await;

        // Converge over the lossy channel. The seed makes the drop/dup/reorder pattern
        // reproducible — a failure here is a real non-convergence bug, never coin-flip.
        let rounds =
            pump_lossy_to_quiescence(&[&a, &b, &c], LossProfile::hostile(), 0xBADC0DE).await;
        // Convergence must be FAST even over the lossy channel — a small bounded number
        // of lossy/clean iterations, nowhere near the driver's runaway cap. A blow-up
        // here would flag a real convergence regression, not lossy noise.
        assert!(
            (1..=50).contains(&rounds),
            "lossy convergence should settle quickly (took {rounds} iterations)"
        );

        // All three replicas hold the same live set, byte-identical.
        let mut files_a = a.list_files().await.unwrap();
        let mut files_b = b.list_files().await.unwrap();
        let mut files_c = c.list_files().await.unwrap();
        files_a.sort();
        files_b.sort();
        files_c.sort();
        let expected = vec![
            "from-a.md".to_string(),
            "from-b.md".to_string(),
            "from-c.md".to_string(),
        ];
        assert_eq!(
            files_a, expected,
            "A converged to the full live set over the lossy channel"
        );
        assert_eq!(
            files_b, expected,
            "B converged to the full live set over the lossy channel"
        );
        assert_eq!(
            files_c, expected,
            "C converged to the full live set over the lossy channel"
        );

        for path in &expected {
            let md_a = read_md(&fs_a, path).await;
            let md_b = read_md(&fs_b, path).await;
            let md_c = read_md(&fs_c, path).await;
            assert_eq!(
                md_a, md_b,
                "{path} is byte-identical on A and B over the lossy channel"
            );
            assert_eq!(
                md_b, md_c,
                "{path} is byte-identical on B and C over the lossy channel"
            );
        }

        // The catalogs converged too — identical Index version vectors across all three.
        assert_eq!(
            a.index().state_vv(),
            b.index().state_vv(),
            "A and B Index version vectors converge"
        );
        assert_eq!(
            b.index().state_vv(),
            c.index().state_vv(),
            "B and C Index version vectors converge"
        );
    }
}

// ===================== harness self-check: the lossy channel really faults =====================

mod harness_lossy_actually_faults {
    use super::*;

    /// Guard the AC-§5 test's premise: over the seed it uses, the hostile loss profile
    /// must actually DROP and DUPLICATE a meaningful number of payloads — otherwise the
    /// "converges over a lossy channel" test would be vacuous (passing because nothing
    /// was ever lost). Replays the same PRNG/profile the test uses and counts faults.
    #[tokio::test]
    async fn hostile_profile_injects_drops_and_dups_over_the_test_seed() {
        let mut rng = DeterministicRng::new(0xBADC0DE);
        let profile = LossProfile::hostile();
        let (mut drops, mut dups) = (0usize, 0usize);
        // Sample the same coin sequence the driver would consume across many deliveries.
        for _ in 0..600 {
            if rng.chance(profile.drop) {
                drops += 1;
            } else if rng.chance(profile.duplicate) {
                dups += 1;
            }
        }
        assert!(
            drops > 50,
            "the hostile profile must drop a meaningful share of payloads (got {drops})"
        );
        assert!(
            dups > 30,
            "the hostile profile must duplicate a meaningful share of payloads (got {dups})"
        );
    }
}
