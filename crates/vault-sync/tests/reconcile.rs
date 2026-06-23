//! Acceptance tests for boot reconciliation (INV-7, FR-7) and the native-move adopt
//! (EC-8/S1).
//!
//! Reconcile is the fs-first pass `Vault::load` runs before the vault opens to remote
//! sync: it documents local filesystem state into the Index. These tests drive it the
//! way a restart does — set up a `.sync/` + on-disk state, then `Vault::load` (which
//! runs reconcile) and assert the heal. The headline properties are the four INV-7
//! arms (adopt / quarantine / report / reindex), the native-move lineage re-attach
//! (zero content re-transferred), the corrupt-state containment (a corrupt Index
//! hard-fails; a single corrupt content doc is skipped), and the S3 `content_version`
//! repair.
//!
//! Everything runs against `InMemoryFs` — no test touches a real vault.
//!
//! The replica/edit/inspection helpers live in the shared [`common`] harness
//! (`tests/common/mod.rs`); this file pulls `one_vault`, `write_and_index`, `uuid_at`,
//! and `materialized_markdown` from there. `AUTHOR` is the single-vault author id.

use std::sync::Arc;

use uuid::Uuid;
use vault_sync::{ContentDoc, FileSystem, InMemoryFs, IndexError, Vault, content_doc_path};

mod common;
use common::*;

// ========================= AC-INV-6/7 — adopt / quarantine / report =========================

mod ac_inv_6_7_reconcile_arms {
    use super::*;

    /// An orphaned content doc — `docs/<uuid>.loro` on disk with NO Index node — is
    /// ADOPTED at the path of its matching `.md`: a node is created, the orphan's
    /// lineage UUID is preserved (not re-minted), and the path is resolved from disk.
    /// This is the fs↔loro divergence heal (a peer's content landed without its node).
    #[tokio::test]
    async fn orphaned_loro_with_matching_md_is_adopted_preserving_lineage_uuid() {
        let (_seed, fs) = one_vault().await;

        // Stage a content doc on disk WITHOUT any Index node: write its `<uuid>.loro`
        // and its materialized `.md`, but never register it. Its UUID is its identity.
        let doc =
            ContentDoc::from_markdown("# Stranded\n\nBody that never got a node.", AUTHOR).unwrap();
        let orphan_uuid = Uuid::parse_str(&doc.doc_id().unwrap()).unwrap();
        fs.atomic_write(
            &content_doc_path(&orphan_uuid),
            &doc.export_snapshot().unwrap(),
        )
        .await
        .unwrap();
        fs.write("notes/stranded.md", doc.to_markdown().as_bytes())
            .await
            .unwrap();

        // Boot reconcile (via load) adopts the orphan at its `.md`'s path.
        let vault = Vault::load(Arc::clone(&fs), AUTHOR).await.unwrap();

        let adopted_uuid = uuid_at(&vault, "notes/stranded.md");
        assert_eq!(
            adopted_uuid, orphan_uuid,
            "the adopted node preserves the orphan's lineage UUID (not a fresh mint)"
        );
        // The orphan's content `.loro` is the SAME file (UUID-addressed) and was not
        // relocated by the adopt.
        assert!(
            fs.exists(&content_doc_path(&orphan_uuid)).await.unwrap(),
            "the orphan's content doc is still at docs/<uuid>.loro"
        );
    }

    /// A tombstoned disk orphan — a `.md` still on disk at a path the Index has
    /// tombstoned — is QUARANTINED to `.trash/`, never resurrected as a live node.
    #[tokio::test]
    async fn tombstoned_md_still_on_disk_is_quarantined_not_resurrected() {
        let (vault, fs) = one_vault().await;

        write_and_index(&vault, &fs, "old/note.md", "delete me\n").await;
        let uuid = uuid_at(&vault, "old/note.md");

        // Tombstone the node but LEAVE the `.md` on disk (the strand). Remove the
        // content `.loro` so it is not an adopt candidate — this isolates the
        // quarantine arm.
        vault.index().delete_node("old/note.md").unwrap();
        vault.save_index().await.unwrap();
        fs.delete(&content_doc_path(&uuid)).await.unwrap();

        // Reload: reconcile must quarantine the strand, not resurrect it.
        let reloaded = Vault::load(Arc::clone(&fs), AUTHOR).await.unwrap();

        assert!(
            reloaded.index().node_for_path("old/note.md").is_none(),
            "the tombstoned path is NOT resurrected as a live node"
        );
        assert!(
            !fs.exists("old/note.md").await.unwrap(),
            "the strand `.md` was removed from its live location"
        );
        assert!(
            fs.exists(".trash/old/note.md").await.unwrap(),
            "the strand `.md` was preserved under .trash/ (reversible quarantine)"
        );
    }

    /// An alive Index node whose backing `.md` is gone from disk is REPORT-ONLY:
    /// reconcile neither recreates the file (resurrection) nor tombstones the node
    /// (deletion-propagation).
    #[tokio::test]
    async fn alive_node_with_missing_md_is_report_only() {
        let (vault, fs) = one_vault().await;

        write_and_index(&vault, &fs, "kept.md", "i exist\n").await;

        // Delete the `.md` from disk but leave the node (and its `.loro`) intact.
        fs.delete("kept.md").await.unwrap();

        let reloaded = Vault::load(Arc::clone(&fs), AUTHOR).await.unwrap();

        // The node is still alive — NOT tombstoned by the missing file.
        assert!(
            reloaded.index().node_for_path("kept.md").is_some(),
            "a missing `.md` does not tombstone its still-alive node"
        );
        // And the `.md` was NOT resurrected from the content doc.
        assert!(
            !fs.exists("kept.md").await.unwrap(),
            "reconcile does not recreate a missing `.md` (report-only)"
        );
    }

    /// A new `.md` with no node and no content doc is indexed (Flow-1) — the
    /// brand-new-file arm.
    #[tokio::test]
    async fn brand_new_md_on_disk_is_indexed() {
        let (_seed, fs) = one_vault().await;

        // A `.md` appeared on disk while the vault was off (no node, no `.loro`).
        fs.write("fresh/idea.md", b"# Idea\n\nNew while offline.\n")
            .await
            .unwrap();

        let vault = Vault::load(Arc::clone(&fs), AUTHOR).await.unwrap();

        let node = vault
            .index()
            .node_for_path("fresh/idea.md")
            .expect("the new file was indexed with a node");
        let uuid = vault
            .index()
            .node_uuid(&node)
            .expect("the node carries a UUID");
        assert!(
            fs.exists(&content_doc_path(&uuid)).await.unwrap(),
            "indexing the new file wrote its content doc at docs/<uuid>.loro"
        );
    }
}

// ========================= AC-S1 — native-move adopt =========================

mod ac_s1_native_move_adopt {
    use super::*;

    /// Stage a native move on disk: a document materialized at `old` is deleted
    /// (node tombstoned, `.md` removed) but its content `docs/<uuid>.loro` survives,
    /// and a `.md` with byte-identical content appears at `new`. Returns the moved
    /// document's UUID and the bytes of its content doc (to assert it is never
    /// rewritten — the zero-content guarantee).
    async fn stage_native_move(fs: &Fs, old: &str, new: &str, content: &str) -> (Uuid, Vec<u8>) {
        let setup = Vault::init(Arc::clone(fs), AUTHOR).await.unwrap();
        write_and_index(&setup, fs, old, content).await;
        let uuid = uuid_at(&setup, old);
        // The exact bytes a synced peer holds — capture before the move so we can
        // prove the move never rewrites them.
        let loro_before = fs.read(&content_doc_path(&uuid)).await.unwrap();
        let rendered = materialized_markdown(&setup, old).await;

        // Tombstone the source node and remove the source `.md`, but KEEP the content
        // doc (a move relocates the `.md`, not the path-independent content `.loro`).
        setup.index().delete_node(old).unwrap();
        setup.save_index().await.unwrap();
        fs.delete(old).await.unwrap();

        // The destination `.md` carries byte-identical content (a pure move).
        fs.write(new, rendered.as_bytes()).await.unwrap();

        (uuid, loro_before)
    }

    /// A `delete(old)` + `create(new)` whose new content matches the orphan AND whose
    /// orphan's tombstoned path is `old` re-attaches the SAME UUID at `new` — the
    /// move's lineage is preserved and zero content is re-transferred (the content
    /// `.loro` is byte-identical before and after, so a synced peer's version-vector
    /// comparison ships nothing).
    #[tokio::test]
    async fn matching_content_at_new_reattaches_uuid_with_zero_content_rewrite() {
        let fs = Arc::new(InMemoryFs::new());
        let (moved_uuid, loro_before) = stage_native_move(
            &fs,
            "inbox/draft.md",
            "archive/draft.md",
            "# Draft\n\nProse.\n",
        )
        .await;

        let vault = Vault::load(Arc::clone(&fs), AUTHOR).await.unwrap();

        // The new path carries the SAME UUID — lineage re-attached, not re-minted.
        assert_eq!(
            uuid_at(&vault, "archive/draft.md"),
            moved_uuid,
            "the moved document keeps its UUID at the new path (lineage re-attach)"
        );
        // The old path is not resurrected.
        assert!(
            vault.index().node_for_path("inbox/draft.md").is_none(),
            "the move source path is not resurrected"
        );
        // Zero content re-transfer: the content `.loro` was never rewritten, so a peer
        // already holding this document at this version receives nothing.
        let loro_after = fs.read(&content_doc_path(&moved_uuid)).await.unwrap();
        assert_eq!(
            loro_before, loro_after,
            "the move re-attaches lineage WITHOUT rewriting the content doc (zero content)"
        );
    }

    /// When the new `.md`'s content does NOT match the orphan, no move-adopt fires:
    /// the new file is a genuinely new document with a FRESH UUID, and the orphan is
    /// reported (not mis-adopted).
    #[tokio::test]
    async fn non_matching_content_mints_fresh_uuid_no_mis_adopt() {
        let fs = Arc::new(InMemoryFs::new());
        let (moved_uuid, _loro) = stage_native_move(
            &fs,
            "inbox/draft.md",
            "archive/draft.md",
            "# Draft\n\nProse.\n",
        )
        .await;

        // Overwrite the destination with DIFFERENT content — it is not the moved doc.
        fs.write(
            "archive/draft.md",
            b"# Unrelated\n\nDifferent content entirely.\n",
        )
        .await
        .unwrap();

        let vault = Vault::load(Arc::clone(&fs), AUTHOR).await.unwrap();

        let new_uuid = uuid_at(&vault, "archive/draft.md");
        assert_ne!(
            new_uuid, moved_uuid,
            "non-matching content is a fresh document with a fresh UUID, never the orphan's"
        );
    }

    /// A same-content create with NO preceding delete (the content coincides with a
    /// still-LIVE document, not an orphan) is NOT adopted onto that document: it is a
    /// fresh file with a fresh UUID. This is the window bound — only an *orphaned*
    /// content doc is an adopt candidate, never a live one.
    #[tokio::test]
    async fn same_content_create_without_delete_is_not_adopted_window_bound() {
        let (vault, fs) = one_vault().await;

        // A live document at `original.md` — its node is NOT deleted.
        write_and_index(&vault, &fs, "original.md", "# Shared\n\nSame body.\n").await;
        let original_uuid = uuid_at(&vault, "original.md");
        let rendered = materialized_markdown(&vault, "original.md").await;

        // A new file with byte-identical content appears at a different path.
        fs.write("copy.md", rendered.as_bytes()).await.unwrap();

        let reloaded = Vault::load(Arc::clone(&fs), AUTHOR).await.unwrap();

        // The copy is a FRESH document (no orphan existed to adopt — the matching doc
        // is still live), and the original is untouched.
        assert_ne!(
            uuid_at(&reloaded, "copy.md"),
            original_uuid,
            "a same-content create with no orphan is fresh, not adopted onto the live doc"
        );
        assert_eq!(
            uuid_at(&reloaded, "original.md"),
            original_uuid,
            "the live original document is untouched"
        );
    }
}

// ========================= AC-§8 — edge cases (EC-3 / EC-9 / EC-10) =========================

mod ac_section_8_edge_cases {
    use super::*;

    /// EC-9: loading a vault whose Index CRDT is corrupt HARD-FAILS with a clean
    /// error — it must NOT silently fall back to an empty Index (which would re-index
    /// every file with fresh UUIDs and diverge from peers). The hard-fail happens in
    /// `load_index`, before reconcile runs.
    #[tokio::test]
    async fn corrupt_index_hard_fails_not_empty_fallback() {
        let (vault, fs) = one_vault().await;
        write_and_index(&vault, &fs, "note.md", "body\n").await;

        // Corrupt the persisted Index CRDT.
        fs.write(".sync/index.loro", b"definitely not a loro snapshot")
            .await
            .unwrap();

        let result = Vault::load(Arc::clone(&fs), AUTHOR).await;
        assert!(
            matches!(result, Err(IndexError::CorruptIndex(_))),
            "a corrupt Index must hard-fail with CorruptIndex, got {:?}",
            result.err()
        );
    }

    /// EC-10 / NFR-6: a single corrupt content doc is CONTAINED — reconcile skips it
    /// and the rest of the pass proceeds. A valid orphan alongside the corrupt one is
    /// still adopted, and load succeeds.
    #[tokio::test]
    async fn corrupt_single_content_doc_is_skipped_rest_proceed() {
        let (_seed, fs) = one_vault().await;

        // A VALID orphan (content doc + matching `.md`, no node) that should adopt.
        let good = ContentDoc::from_markdown("# Good\n\nValid orphan body.\n", AUTHOR).unwrap();
        let good_uuid = Uuid::parse_str(&good.doc_id().unwrap()).unwrap();
        fs.atomic_write(
            &content_doc_path(&good_uuid),
            &good.export_snapshot().unwrap(),
        )
        .await
        .unwrap();
        fs.write("good.md", good.to_markdown().as_bytes())
            .await
            .unwrap();

        // A CORRUPT content doc (garbage bytes) at a valid-UUID filename, plus a `.md`.
        // from_bytes will fail on it; reconcile must skip it without aborting.
        let bad_uuid = Uuid::new_v4();
        fs.atomic_write(&content_doc_path(&bad_uuid), b"corrupt loro bytes")
            .await
            .unwrap();
        fs.write("bad.md", b"# Bad\n\nIts content doc is corrupt.\n")
            .await
            .unwrap();

        // Reconcile completes (does not abort on the corrupt doc).
        let vault = Vault::load(Arc::clone(&fs), AUTHOR).await.unwrap();

        // The valid orphan adopted, preserving its UUID.
        assert_eq!(
            uuid_at(&vault, "good.md"),
            good_uuid,
            "the valid orphan is adopted despite a corrupt sibling content doc"
        );
        // The corrupt doc's UUID was never registered as a node (it was skipped).
        assert!(
            vault.index().find_node_by_uuid(&bad_uuid).is_none(),
            "the corrupt content doc's UUID is not registered (it was skipped)"
        );
    }

    /// EC-3: a crash mid-apply that left a live Index node with no content doc on disk
    /// is reconciled without crashing (the node-without-`.loro` arm is non-destructive),
    /// and an idempotent re-sync from the peer that still holds the document recovers
    /// it (re-delivery converges).
    #[tokio::test]
    async fn crash_mid_apply_node_without_content_recovers_on_resync() {
        // A holds a real document.
        let fs_a = Arc::new(InMemoryFs::new());
        let a = Vault::init(Arc::clone(&fs_a), AUTHOR).await.unwrap();
        write_and_index(&a, &fs_a, "doc.md", "real content\n").await;

        // B receives ONLY the Index (the node lands) but not the document — a partial
        // apply, as if a crash hit between the Index write and the document write. The
        // Flow-2 gate means no `.md`/`.loro` materialized, so B has a live node with no
        // content doc on disk.
        let fs_b = Arc::new(InMemoryFs::new());
        let index_only = vault_sync::SyncMessage::SyncResponse {
            index_updates: Some(a.index().export_snapshot().unwrap()),
            document_updates: std::collections::HashMap::new(),
        };
        {
            let b = Vault::init(Arc::clone(&fs_b), 0x0202_0202_0202_0202)
                .await
                .unwrap();
            b.process_message(&bincode::serialize(&index_only).unwrap())
                .await
                .unwrap();
            b.save_index().await.unwrap();
            assert!(
                b.index().node_for_path("doc.md").is_some(),
                "precondition: B has the node from the partial apply"
            );
            assert!(
                !fs_b.exists("doc.md").await.unwrap(),
                "precondition: B never materialized the document (Flow-2 gate held it)"
            );
        }

        // Reload B: boot reconcile must not crash on the node-without-content state.
        let b = Vault::load(Arc::clone(&fs_b), 0x0202_0202_0202_0202)
            .await
            .unwrap();

        // A full re-sync delivers the document (idempotent re-delivery) and B converges.
        // Driven to termination through the complete handshake: with the digest
        // fast-path (§6.2) a real change-sync runs four messages — the content B needs
        // rides A's FINAL SyncResponse, one leg later than the old three-message flow —
        // so the full pump (not a fixed-leg hand-drive) is what delivers it.
        full_sync(&b, &a).await;

        assert!(
            fs_b.exists("doc.md").await.unwrap(),
            "the document materializes on B after an idempotent re-sync"
        );
        let md = String::from_utf8(fs_b.read("doc.md").await.unwrap()).unwrap();
        assert!(
            md.contains("real content"),
            "B's recovered content is correct: {md:?}"
        );
    }
}

// ===================== content_version table rebuild on boot =====================

mod content_version_table_rebuild {
    use super::*;

    /// The `content_version` fingerprint is a per-replica-LOCAL transient table (no peer
    /// can write it), rebuilt from each content doc's `.loro` on every boot. A fresh
    /// `Vault::load` over the same filesystem reconstructs the table from disk, so the
    /// reloaded vault yields the SAME catalog digest as before the drop AND the same
    /// per-document fingerprint — proving `rebuild_content_versions` covers every live
    /// doc and recomputes the correct value.
    ///
    /// This is the boot-rebuild regression guard (it replaces the S3 "stale persisted
    /// meta" repair test — under the local-table model there is no persisted fingerprint
    /// to go stale, so "a fresh load reproduces the table from disk" is the property that
    /// matters). If `rebuild_content_versions` were absent or broken, the reloaded table
    /// would be empty/partial and the digest would diverge from the pre-drop digest (the
    /// per-document entries would be missing), failing the first assertion.
    #[tokio::test]
    async fn boot_rebuilds_content_version_table_from_disk() {
        let fs = Arc::new(InMemoryFs::new());

        let digest_before;
        let note_fingerprint_before;
        {
            let vault = Vault::init(Arc::clone(&fs), AUTHOR).await.unwrap();
            write_and_index(&vault, &fs, "note.md", "# Note\n\nBody.\n").await;
            write_and_index(&vault, &fs, "other/deep.md", "# Deep\n\nMore.\n").await;

            // The whole-vault digest and one doc's fingerprint, both derived from the
            // table populated incrementally during the local writes.
            digest_before = vault.catalog_digest();
            let node = vault.index().node_for_path("note.md").unwrap();
            note_fingerprint_before = vault
                .index()
                .node_content_version(&node)
                .expect("a freshly-written doc has a content_version");
        }

        // Drop the vault and load a fresh one over the same fs — the table starts empty
        // and is rebuilt by boot reconcile from each `.loro` on disk.
        let reloaded = Vault::load(Arc::clone(&fs), AUTHOR).await.unwrap();

        // The whole-vault digest is reproduced exactly: every live doc's fingerprint was
        // rebuilt from disk (an absent/partial rebuild would drop entries and diverge).
        assert_eq!(
            reloaded.catalog_digest(),
            digest_before,
            "a fresh load rebuilds the content_version table from disk and reproduces the digest"
        );

        // A specific entry round-trips, not just the aggregate digest.
        let node = reloaded.index().node_for_path("note.md").unwrap();
        assert_eq!(
            reloaded.index().node_content_version(&node),
            Some(note_fingerprint_before),
            "the rebuilt table holds the correct per-document fingerprint after reload"
        );
    }
}

// ========================= P4f-2b — boot journal re-stitch (crash recovery) =========================

mod ac_p4f_2b_journal_restitch {
    //! Boot crash-recovery for the move-coalescer (P4f-2b-i): a journaled native-move
    //! DELETE whose original UUID STILL has a live node AND whose content matches an
    //! orphaned on-disk `.md` is RE-STITCHED inside reconcile — the original UUID
    //! re-attached at the new path via a single `move_node` on a still-live source,
    //! BEFORE reconcile's per-file loop would mint a fresh node for that orphan.
    //!
    //! Every test drives the REAL `Vault::load_with_journal` (never a hand-stitched
    //! Index — that would be a false green). The crash state is staged with
    //! [`stage_buffered_move_crash`], the sibling of `stage_native_move` that leaves
    //! the OLD node LIVE (the distinguishing input: the move's delete buffered but
    //! never committed, so the `.loro` is NOT orphaned and `adopt_orphans` cannot see
    //! it — exactly the gap the journal fills).
    //!
    //! The structural reason in-reconcile recovery has no data-loss window: the
    //! recovery path contains NO `delete_file(new_path)`, so the `{old alive, new
    //! tombstoned}` crash-intermediate the rejected post-load reclaim would persist is
    //! unconstructable here. `mid_reclaim_crash_does_not_lose_file` pins that.

    use super::*;
    use vault_sync::{JournalReStitch, content_hash};

    /// Stage the exact mid-window crash state the move-coalescer leaves on disk: a
    /// document materialized at `old` whose native-move delete BUFFERED (the node stays
    /// LIVE — the delete never committed) while the `.md` already relocated to `new` on
    /// disk. The old `.md` is gone; a byte-identical `.md` is at `new`; the content
    /// `docs/<uuid>.loro` survives (a move never touches the path-independent content).
    ///
    /// The key difference from `stage_native_move`: it does NOT `delete_node(old)`. The
    /// old node remains live, so its content `.loro` is NOT an orphan and `adopt_orphans`
    /// structurally cannot re-attach it — only the journal-driven re-stitch can. Returns
    /// the moved document's `(uuid, content_hash)` so the test builds the
    /// `JournalReStitch` record the daemon would.
    async fn stage_buffered_move_crash(
        fs: &Fs,
        old: &str,
        new: &str,
        content: &str,
    ) -> (Uuid, [u8; 32]) {
        let setup = Vault::init(Arc::clone(fs), AUTHOR).await.unwrap();
        write_and_index(&setup, fs, old, content).await;
        let uuid = uuid_at(&setup, old);

        // The content hash in the journal's domain: `content_hash(ContentDoc)` over the
        // document's materialized markdown — the SAME expression the re-stitch hashes the
        // on-disk orphan with, and the same the daemon journaled at delete-buffer time.
        let rendered = materialized_markdown(&setup, old).await;
        let doc = ContentDoc::from_markdown(&rendered, AUTHOR).unwrap();
        let hash = content_hash(&doc);

        // The mid-window crash: the OLD node stays LIVE (no delete_node — the delete
        // buffered but never committed), the old `.md` is gone, a byte-identical `.md`
        // is at `new`. The content `.loro` is untouched (path-independent).
        fs.delete(old).await.unwrap();
        fs.write(new, rendered.as_bytes()).await.unwrap();

        (uuid, hash)
    }

    /// Stage TWO independent buffered-move crashes against a SINGLE Index — the state a
    /// crash leaves when two distinct moves had buffered their deletes. Both old nodes
    /// stay LIVE in the one persisted Index (so the re-stitch sees both as live sources),
    /// both old `.md`s are gone, and a byte-identical `.md` for each is at its new path.
    ///
    /// Cannot be expressed as two `stage_buffered_move_crash` calls on the same fs: that
    /// helper re-runs `Vault::init`, which builds a FRESH Index and overwrites
    /// `.sync/index.loro`, wiping the first move's live node. The two moves must be
    /// indexed into ONE Index, then crashed together. Returns each move's
    /// `(uuid, content_hash)` for the journal records.
    #[allow(clippy::type_complexity)]
    async fn stage_two_buffered_move_crashes(
        fs: &Fs,
        a: (&str, &str, &str),
        b: (&str, &str, &str),
    ) -> ((Uuid, [u8; 32]), (Uuid, [u8; 32])) {
        let (a_old, a_new, a_content) = a;
        let (b_old, b_new, b_content) = b;

        let setup = Vault::init(Arc::clone(fs), AUTHOR).await.unwrap();
        write_and_index(&setup, fs, a_old, a_content).await;
        write_and_index(&setup, fs, b_old, b_content).await;

        let a_uuid = uuid_at(&setup, a_old);
        let b_uuid = uuid_at(&setup, b_old);

        let a_rendered = materialized_markdown(&setup, a_old).await;
        let b_rendered = materialized_markdown(&setup, b_old).await;
        let a_hash = content_hash(&ContentDoc::from_markdown(&a_rendered, AUTHOR).unwrap());
        let b_hash = content_hash(&ContentDoc::from_markdown(&b_rendered, AUTHOR).unwrap());

        // Both deletes buffered (nodes stay LIVE), both old `.md`s gone, both new `.md`s
        // on disk. The persisted Index from the writes above carries both live nodes.
        fs.delete(a_old).await.unwrap();
        fs.delete(b_old).await.unwrap();
        fs.write(a_new, a_rendered.as_bytes()).await.unwrap();
        fs.write(b_new, b_rendered.as_bytes()).await.unwrap();

        ((a_uuid, a_hash), (b_uuid, b_hash))
    }

    /// HEADLINE: a buffered-move crash re-stitches the SAME UUID at the new path on
    /// boot — the move's lineage is recovered, not re-minted as a fresh document.
    ///
    /// This is the journal-augmented counterpart of
    /// `same_content_create_without_delete_is_not_adopted_window_bound`: identical disk
    /// shape (a live old node + a content-matching new `.md`), but the journal flips the
    /// outcome from "fresh UUID" to "same-UUID move". It must pass for the RIGHT
    /// structural reason — reconcile re-attached the UUID via `move_node` BEFORE any
    /// fresh mint, with zero content re-transfer (the `.loro` is byte-identical).
    #[tokio::test]
    async fn crash_then_boot_restitches_move_same_uuid() {
        let fs = Arc::new(InMemoryFs::new());
        let (uuid, hash) = stage_buffered_move_crash(
            &fs,
            "inbox/draft.md",
            "archive/draft.md",
            "# Draft\n\nProse.\n",
        )
        .await;
        let loro_before = fs.read(&content_doc_path(&uuid)).await.unwrap();

        let journaled = vec![JournalReStitch {
            uuid,
            old_path: "inbox/draft.md".to_string(),
            content_hash: hash,
        }];
        let vault = Vault::load_with_journal(Arc::clone(&fs), AUTHOR, Some(&journaled))
            .await
            .unwrap();

        // The new path carries the ORIGINAL UUID — re-stitched, not freshly minted.
        assert_eq!(
            uuid_at(&vault, "archive/draft.md"),
            uuid,
            "the buffered move re-stitches its original UUID at the new path"
        );
        // The old path is not resurrected.
        assert!(
            vault.index().node_for_path("inbox/draft.md").is_none(),
            "the move source path is not left live after the re-stitch"
        );
        // The destination content survives intact.
        assert_eq!(
            read_md_str(&fs, "archive/draft.md").await,
            "# Draft\n\nProse.\n",
            "the relocated content is intact at the new path"
        );
        // Zero content re-transfer: the content `.loro` was never rewritten (a move
        // re-attaches lineage structurally; the content doc is path-independent).
        let loro_after = fs.read(&content_doc_path(&uuid)).await.unwrap();
        assert_eq!(
            loro_before, loro_after,
            "the re-stitch re-attaches lineage WITHOUT rewriting the content doc (zero content)"
        );
    }

    /// The review's data-loss proof: the rejected post-load reclaim's `{old alive, new
    /// tombstoned}` intermediate is UNREACHABLE under in-reconcile recovery, so the
    /// renamed file is never lost.
    ///
    /// The rejected design (mint-at-`new` then `delete_file(new)` + `move_node`) could
    /// crash between its two sub-steps, persisting `{old node alive, new node
    /// tombstoned, renamed .md on disk at new}`. The NEXT boot would then quarantine the
    /// `.md` at `new` (its first match arm is `(_, _, true)` -> `quarantine_orphan`,
    /// moving the user's file to `.trash/`) AND tombstone the live old node — the file
    /// vanishes from its live location and a spurious deletion propagates.
    ///
    /// Under in-reconcile recovery this state CANNOT exist: the recovery path contains
    /// NO `delete_file(new)` — it is a single `move_node(old -> new)` on a live source
    /// while `new` has no node yet. With nothing that tombstones `new`, the quarantine
    /// arm that loses the file can never fire on `new`. This test stages precisely the
    /// crash input (live old node, journal listing the delete, `.md` at `new`) and
    /// asserts the file is NOT quarantined, the old node is NOT spuriously tombstoned,
    /// and the file survives at `new` with its UUID.
    #[tokio::test]
    async fn mid_reclaim_crash_does_not_lose_file() {
        let fs = Arc::new(InMemoryFs::new());
        let (uuid, hash) = stage_buffered_move_crash(
            &fs,
            "notes/recipe.md",
            "recipes/recipe.md",
            "# Recipe\n\nMix and bake.\n",
        )
        .await;

        let journaled = vec![JournalReStitch {
            uuid,
            old_path: "notes/recipe.md".to_string(),
            content_hash: hash,
        }];
        let vault = Vault::load_with_journal(Arc::clone(&fs), AUTHOR, Some(&journaled))
            .await
            .unwrap();

        // The file is NOT lost to `.trash/` — the quarantine arm never fires on `new`
        // because nothing in the recovery path tombstones `new` (no `delete_file(new)`).
        assert!(
            !fs.exists(".trash/recipes/recipe.md").await.unwrap(),
            "the renamed file is never quarantined — the data-loss intermediate is unreachable"
        );
        // The file survives at its live location with its original UUID.
        assert_eq!(
            uuid_at(&vault, "recipes/recipe.md"),
            uuid,
            "the file survives at the new path with its original UUID"
        );
        assert_eq!(
            read_md_str(&fs, "recipes/recipe.md").await,
            "# Recipe\n\nMix and bake.\n",
            "the file content is preserved, not lost"
        );
        // The old path is vacated by the move, not left as a live duplicate.
        assert!(
            vault.index().node_for_path("notes/recipe.md").is_none(),
            "the old path is vacated by the move, not left as a live duplicate"
        );
    }

    /// MANDATORY idempotency: booting twice over the same `.sync/`+journal+disk (2b-i
    /// does not empty the journal) makes no further change on the second run — a
    /// mid-recovery crash converges from the intact journal.
    ///
    /// On the second boot the old node has already MOVED to `new`, so the re-stitch's
    /// live-node guard resolves the UUID to a node whose path is now `new != old` and
    /// skips it (and even absent that guard, no orphaned `.md` remains at a different
    /// path with no node). Both reasons make the second run a no-op.
    #[tokio::test]
    async fn boot_runs_twice_is_idempotent() {
        let fs = Arc::new(InMemoryFs::new());
        let (uuid, hash) =
            stage_buffered_move_crash(&fs, "a/one.md", "b/one.md", "# One\n\nFirst body.\n").await;

        let journaled = vec![JournalReStitch {
            uuid,
            old_path: "a/one.md".to_string(),
            content_hash: hash,
        }];

        // First boot: re-stitch fires. The journal is intentionally NOT emptied (that is
        // 2b-ii) — the SAME records are replayed on the second boot.
        let first = Vault::load_with_journal(Arc::clone(&fs), AUTHOR, Some(&journaled))
            .await
            .unwrap();
        assert_eq!(uuid_at(&first, "b/one.md"), uuid);
        drop(first);

        // Second boot over the now-mutated `.sync`+disk with the SAME journal: a no-op.
        let second = Vault::load_with_journal(Arc::clone(&fs), AUTHOR, Some(&journaled))
            .await
            .unwrap();
        assert_eq!(
            uuid_at(&second, "b/one.md"),
            uuid,
            "the second boot leaves the re-stitched UUID in place (idempotent)"
        );
        assert!(
            second.index().node_for_path("a/one.md").is_none(),
            "the old path stays vacated on the second boot (no double-move resurrection)"
        );
        assert_eq!(
            read_md_str(&fs, "b/one.md").await,
            "# One\n\nFirst body.\n",
            "content unchanged across the idempotent second boot"
        );
    }

    /// The live-node gate (step 1): a journaled record whose UUID has NO live node
    /// (its delete already committed — the node is tombstoned) is SKIPPED. The re-stitch
    /// never acts on a tombstoned UUID.
    ///
    /// Here the old node is tombstoned (simulating "the delete already finalized on a
    /// prior boot"). `find_node_by_uuid` excludes tombstones, so step 1 resolves `None`
    /// and skips. The `.md` at `new` is then handled by the normal reconcile path —
    /// adopted by `adopt_orphans` (its `.loro` IS now orphaned, since the node is
    /// tombstoned), which is the EC-8 native-move-adopt, NOT the journal re-stitch. The
    /// point of this test is that the re-stitch did not error or mis-act on the
    /// tombstoned UUID, not which arm ultimately re-homes the file.
    #[tokio::test]
    async fn restitch_skips_when_uuid_has_no_live_node() {
        // A clean crash state, then tombstone the old node directly (the delete already
        // committed on a prior boot), so the journaled UUID has no live node.
        let fs = Arc::new(InMemoryFs::new());
        let setup = Vault::init(Arc::clone(&fs), AUTHOR).await.unwrap();
        write_and_index(&setup, &fs, "src/note.md", "# Note\n\nBody here.\n").await;
        let uuid = uuid_at(&setup, "src/note.md");
        let rendered = materialized_markdown(&setup, "src/note.md").await;
        let hash = content_hash(&ContentDoc::from_markdown(&rendered, AUTHOR).unwrap());
        // Tombstone (delete) the old node — the delete is now committed.
        setup.index().delete_node("src/note.md").unwrap();
        setup.save_index().await.unwrap();
        fs.delete("src/note.md").await.unwrap();
        fs.write("dst/note.md", rendered.as_bytes()).await.unwrap();
        drop(setup);

        let journaled = vec![JournalReStitch {
            uuid,
            old_path: "src/note.md".to_string(),
            content_hash: hash,
        }];
        // Boot must not panic/error on the tombstoned UUID — the re-stitch skips it.
        let vault = Vault::load_with_journal(Arc::clone(&fs), AUTHOR, Some(&journaled))
            .await
            .unwrap();

        // The tombstoned old path stays tombstoned (no live node re-created there) — the
        // re-stitch did not act on the dead UUID. The file at `dst` is re-homed by the
        // normal path (adopt_orphans, since the `.loro` is now a genuine orphan) — EC-8,
        // not the journal re-stitch.
        assert!(
            vault.index().node_for_path("src/note.md").is_none(),
            "the tombstoned old path stays tombstoned — the re-stitch did not act on it"
        );
        // The moved file is still re-homed: even though the re-stitch SKIPPED the
        // tombstoned UUID, `adopt_orphans` adopts the now-genuinely-orphaned `.loro` at
        // `dst`, so the file is not quarantined or lost — the skip is a hand-off, not a
        // dropped file.
        assert!(
            vault.index().node_for_path("dst/note.md").is_some(),
            "moved file is re-homed by adopt_orphans even when re-stitch skips the tombstoned UUID"
        );
    }

    /// C-S1 — content-hash collision: when TWO orphaned `.md` files share the journaled
    /// move's content hash, EXACTLY ONE inherits the journaled (original) UUID and the
    /// OTHER gets a fresh, distinct UUID. The re-stitch claims a single winner and never
    /// double-attaches the lineage onto both, nor leaves both un-homed.
    ///
    /// WHICH of the two wins is NOT asserted — selection among equal-hash candidates is
    /// non-deterministic by design (the scan is over an unordered `HashSet`, mirroring
    /// `adopt_orphans`). The invariant under test is structural: one and only one node
    /// carries the journaled UUID, both paths end up live, and the two UUIDs differ (no
    /// duplicate lineage minted). The non-winner is indexed as a brand-new file by the
    /// per-file loop, so it has its OWN UUID.
    #[tokio::test]
    async fn restitch_two_equal_content_orphans_exactly_one_gets_uuid() {
        let fs = Arc::new(InMemoryFs::new());
        let (uuid, hash) = stage_buffered_move_crash(
            &fs,
            "inbox/note.md",
            "a/note.md",
            "# Note\n\nIdentical body.\n",
        )
        .await;

        // A SECOND orphaned `.md` at a DIFFERENT path with byte-identical content (so the
        // same `content_hash`) and no node — a second equal-hash candidate competing for
        // the single journaled lineage.
        fs.write("b/note.md", "# Note\n\nIdentical body.\n".as_bytes())
            .await
            .unwrap();

        let journaled = vec![JournalReStitch {
            uuid,
            old_path: "inbox/note.md".to_string(),
            content_hash: hash,
        }];
        let vault = Vault::load_with_journal(Arc::clone(&fs), AUTHOR, Some(&journaled))
            .await
            .unwrap();

        // Both equal-content paths end up with live nodes — neither orphan is dropped or
        // quarantined.
        let uuid_a = uuid_at(&vault, "a/note.md");
        let uuid_b = uuid_at(&vault, "b/note.md");

        // Exactly one of the two inherits the journaled UUID — never both, never neither.
        let a_won = uuid_a == uuid;
        let b_won = uuid_b == uuid;
        assert!(
            a_won ^ b_won,
            "exactly one equal-content orphan inherits the journaled UUID \
             (a_won={a_won}, b_won={b_won}, journaled={uuid})"
        );
        // The two nodes carry DISTINCT UUIDs — the lineage was not minted twice. The
        // non-winner is a fresh document indexed by the per-file loop.
        assert_ne!(
            uuid_a, uuid_b,
            "the two equal-content orphans get distinct UUIDs (no duplicate lineage)"
        );
    }

    /// C-S2 — multi-record: TWO distinct journaled moves (distinct content) in ONE
    /// `journaled` slice each re-stitch INDEPENDENTLY to their own content-matching
    /// orphan — A's UUID lands at A's new path, B's at B's, with no duplicate nodes and
    /// both old paths vacated. Pins that the per-record loop re-stitches every record,
    /// not just the first.
    #[tokio::test]
    async fn restitch_multi_record_each_move_restitched() {
        let fs = Arc::new(InMemoryFs::new());
        // Two buffered-move crashes with DISTINCT content, staged into ONE Index (a single
        // `Vault::init`) so BOTH old nodes are live when reconcile runs — see the helper.
        let ((uuid_a, hash_a), (uuid_b, hash_b)) = stage_two_buffered_move_crashes(
            &fs,
            ("x.md", "a/x.md", "# X\n\nAlpha body.\n"),
            ("y.md", "b/y.md", "# Y\n\nBravo body.\n"),
        )
        .await;

        // Distinct content means distinct UUIDs and distinct hashes — the two records do
        // not compete for the same orphan.
        assert_ne!(
            uuid_a, uuid_b,
            "precondition: the two moves are distinct documents"
        );

        let journaled = vec![
            JournalReStitch {
                uuid: uuid_a,
                old_path: "x.md".to_string(),
                content_hash: hash_a,
            },
            JournalReStitch {
                uuid: uuid_b,
                old_path: "y.md".to_string(),
                content_hash: hash_b,
            },
        ];
        let vault = Vault::load_with_journal(Arc::clone(&fs), AUTHOR, Some(&journaled))
            .await
            .unwrap();

        // Each record re-stitched its OWN move: the original UUID lands at the new path.
        assert_eq!(
            uuid_at(&vault, "a/x.md"),
            uuid_a,
            "record A re-stitches its original UUID at a/x.md"
        );
        assert_eq!(
            uuid_at(&vault, "b/y.md"),
            uuid_b,
            "record B re-stitches its original UUID at b/y.md"
        );
        // Both old paths are vacated by their moves — no live duplicate left behind.
        assert!(
            vault.index().node_for_path("x.md").is_none(),
            "record A's old path is vacated"
        );
        assert!(
            vault.index().node_for_path("y.md").is_none(),
            "record B's old path is vacated"
        );
    }

    /// No-disk-evidence (step 3): a journaled delete whose old node is live but with NO
    /// content-matching `.md` at any orphaned path leaves the old node LIVE and untouched
    /// — the lib re-stitch never tombstones (the "no move completed -> commit the delete"
    /// decision is the daemon's, 2b-ii).
    ///
    /// The create half never landed on disk, so there is nothing to re-attach to. The
    /// re-stitch must do NOTHING: the old node stays live at `old_path` (reconcile's
    /// report-only "alive node, missing `.md`" arm covers the now-fileless live node;
    /// the lib never deletes it).
    #[tokio::test]
    async fn restitch_no_match_leaves_old_node_live() {
        let fs = Arc::new(InMemoryFs::new());
        let setup = Vault::init(Arc::clone(&fs), AUTHOR).await.unwrap();
        write_and_index(&setup, &fs, "keep/orig.md", "# Orig\n\nThe body.\n").await;
        let uuid = uuid_at(&setup, "keep/orig.md");
        let rendered = materialized_markdown(&setup, "keep/orig.md").await;
        let hash = content_hash(&ContentDoc::from_markdown(&rendered, AUTHOR).unwrap());
        // The old node stays LIVE (delete buffered, never committed), the old `.md` is
        // gone — but NO matching `.md` ever landed at any new path (the create half was
        // lost in the crash).
        fs.delete("keep/orig.md").await.unwrap();
        setup.save_index().await.unwrap();
        drop(setup);

        let journaled = vec![JournalReStitch {
            uuid,
            old_path: "keep/orig.md".to_string(),
            content_hash: hash,
        }];
        let vault = Vault::load_with_journal(Arc::clone(&fs), AUTHOR, Some(&journaled))
            .await
            .unwrap();

        // No disk evidence of the move -> the lib re-stitch did nothing: the old node is
        // STILL LIVE at its original path (the lib never tombstones — finalize is the
        // daemon's job in 2b-ii).
        assert_eq!(
            uuid_at(&vault, "keep/orig.md"),
            uuid,
            "with no matching .md, the old node stays live and keeps its UUID (lib never finalizes)"
        );
    }

    /// `None` is byte-identical to `Vault::load` (the backward-compat guarantee): a
    /// brand-new `.md` is indexed by reconcile's normal arm, exactly as the bare load
    /// does. The cheap proof that the additive `journaled` param's `None` is the identity.
    #[tokio::test]
    async fn none_journal_is_byte_identical_to_load() {
        let fs = Arc::new(InMemoryFs::new());
        {
            let setup = Vault::init(Arc::clone(&fs), AUTHOR).await.unwrap();
            drop(setup);
        }
        // A brand-new `.md` on disk with no index node.
        fs.write("fresh.md", b"# Fresh\n\nNew content.\n")
            .await
            .unwrap();

        let vault = Vault::load_with_journal(Arc::clone(&fs), AUTHOR, None)
            .await
            .unwrap();

        // reconcile's normal new-file arm fires under `None`, just as `Vault::load` would.
        assert!(
            vault.index().node_for_path("fresh.md").is_some(),
            "with None journal, reconcile indexes a brand-new file exactly as the bare load does"
        );
    }

    /// `Some(&[])` is also a no-op for the re-stitch (distinct from `None`, but equally
    /// inert): reconcile proceeds normally and the empty slice never invokes the
    /// re-stitch pass. Pins that an empty journal is treated as "nothing to recover".
    #[tokio::test]
    async fn empty_journal_slice_is_noop() {
        let fs = Arc::new(InMemoryFs::new());
        {
            let setup = Vault::init(Arc::clone(&fs), AUTHOR).await.unwrap();
            drop(setup);
        }
        fs.write("also-fresh.md", b"# Also\n\nFresh body.\n")
            .await
            .unwrap();

        let empty: Vec<JournalReStitch> = Vec::new();
        let vault = Vault::load_with_journal(Arc::clone(&fs), AUTHOR, Some(&empty))
            .await
            .unwrap();

        assert!(
            vault.index().node_for_path("also-fresh.md").is_some(),
            "an empty journal slice is a no-op — reconcile indexes the new file normally"
        );
    }
}
