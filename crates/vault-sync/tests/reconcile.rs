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

use std::sync::Arc;

use uuid::Uuid;
use vault_sync::{
    ContentDoc, FileSystem, InMemoryFs, IndexError, Vault, content_doc_path,
    content_version_fingerprint,
};

/// A device's Loro author id (a Loro peer id).
const AUTHOR: u64 = 0x0101_0101_0101_0101;

type Fs = Arc<InMemoryFs>;
type V = Vault<Fs>;

/// Seed an empty initialized vault over a fresh in-memory filesystem, returning the
/// retained `Arc<InMemoryFs>` so a test can stage on-disk state directly.
async fn empty_vault() -> (Fs, V) {
    let fs = Arc::new(InMemoryFs::new());
    let vault = Vault::init(Arc::clone(&fs), AUTHOR).await.unwrap();
    (fs, vault)
}

/// Write `content` to `path` and document it into `vault` (Flow-1), flushing the
/// Index so the registration survives a reload.
async fn write_and_index(vault: &V, fs: &Fs, path: &str, content: &str) {
    fs.write(path, content.as_bytes()).await.unwrap();
    vault.on_file_changed(path).await.unwrap();
    vault.save_index().await.unwrap();
}

/// The UUID a path currently resolves to in `vault`'s Index (panics if no node).
fn uuid_at(vault: &V, path: &str) -> Uuid {
    let node = vault
        .index()
        .node_for_path(path)
        .unwrap_or_else(|| panic!("no index node for {path}"));
    vault
        .index()
        .node_uuid(&node)
        .unwrap_or_else(|| panic!("node for {path} carries no UUID"))
}

/// The canonical materialized markdown of the document currently at `path` — the
/// exact bytes the document renders to, used to stage a "matching" `.md` at another
/// path (a native move leaves byte-identical content).
async fn materialized_markdown(vault: &V, path: &str) -> String {
    vault.get_document(path).await.unwrap().to_markdown()
}

// ========================= AC-INV-6/7 — adopt / quarantine / report =========================

mod ac_inv_6_7_reconcile_arms {
    use super::*;

    /// An orphaned content doc — `docs/<uuid>.loro` on disk with NO Index node — is
    /// ADOPTED at the path of its matching `.md`: a node is created, the orphan's
    /// lineage UUID is preserved (not re-minted), and the path is resolved from disk.
    /// This is the fs↔loro divergence heal (a peer's content landed without its node).
    #[tokio::test]
    async fn orphaned_loro_with_matching_md_is_adopted_preserving_lineage_uuid() {
        let (fs, _seed) = empty_vault().await;

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
        let (fs, vault) = empty_vault().await;

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
        let (fs, vault) = empty_vault().await;

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
        let (fs, _seed) = empty_vault().await;

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
        let (fs, vault) = empty_vault().await;

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
        let (fs, vault) = empty_vault().await;
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
        let (fs, _seed) = empty_vault().await;

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
        let request = b.prepare_request().await.unwrap();
        let exchange = a.process_message(&request).await.unwrap().reply.unwrap();
        let after = b.process_message(&exchange).await.unwrap();
        if let Some(reply) = after.reply {
            a.process_message(&reply).await.unwrap();
        }

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

// ========================= S3 — content_version boot repair =========================

mod s3_content_version_repair {
    use super::*;

    /// A node whose denormalized `content_version` is stale relative to its content
    /// doc's actual `state_vv()` (a crash between a content commit and the meta update)
    /// is REPAIRED by boot reconcile: the fingerprint is recomputed from the content
    /// doc and persisted, so the compare digest (P3) reads a fingerprint that matches.
    #[tokio::test]
    async fn stale_content_version_is_recomputed_and_persisted_on_boot() {
        let fs = Arc::new(InMemoryFs::new());
        let expected_fingerprint;
        {
            let vault = Vault::init(Arc::clone(&fs), AUTHOR).await.unwrap();
            write_and_index(&vault, &fs, "note.md", "# Note\n\nBody.\n").await;

            let node = vault.index().node_for_path("note.md").unwrap();
            // The correct fingerprint, derived from the document's actual version.
            let doc = vault.get_document("note.md").await.unwrap();
            expected_fingerprint = content_version_fingerprint(&doc.version());

            // Corrupt the denormalized cache: a stale (all-zero) fingerprint, as a
            // crash between the content commit and the meta update would leave.
            vault
                .index()
                .set_content_version(&node, &[0u8; 32])
                .unwrap();
            vault.save_index().await.unwrap();

            // Sanity: the persisted fingerprint is now genuinely stale.
            assert_ne!(
                vault.index().node_content_version(&node),
                Some(expected_fingerprint),
                "precondition: the persisted content_version is stale"
            );
        }

        // Reload: boot reconcile recomputes the fingerprint from the content doc.
        let reloaded = Vault::load(Arc::clone(&fs), AUTHOR).await.unwrap();
        let node = reloaded.index().node_for_path("note.md").unwrap();
        assert_eq!(
            reloaded.index().node_content_version(&node),
            Some(expected_fingerprint),
            "boot reconcile repaired the stale content_version to match the content doc"
        );
    }
}
