//! Acceptance tests for the `Vault<F>` handle and Flow-1 (local write, fs-as-truth).
//!
//! These drive the public `Vault<F>` API the way a consuming daemon will: a `.md`
//! file is written to the filesystem, `on_file_changed` documents it into the
//! catalog, and the document's content `.loro` is addressed by its minted UUID
//! (`docs/<uuid>.loro`). The headline properties under test are UUID identity (an
//! edit keeps the same UUID; a move keeps the same UUID and rewrites zero content)
//! and echo-safety (a no-op write rewrites nothing and reports no change).
//!
//! Everything runs against `InMemoryFs` — no test touches a real vault.

use std::sync::Arc;
use uuid::Uuid;
use vault_sync::{FileSystem, InMemoryFs, Vault, content_doc_path};

/// A device's Loro author id (a Loro peer id).
const AUTHOR: u64 = 0x0101_0101_0101_0101;

/// Seed an empty initialized vault over a fresh in-memory filesystem.
///
/// Returns the `Arc<InMemoryFs>` (so the test can inspect on-disk bytes directly)
/// alongside the vault. `Arc` because `Vault::init` takes the fs by value; sharing
/// it lets the test read `docs/<uuid>.loro` to assert what Flow-1 wrote.
async fn empty_vault() -> (Arc<InMemoryFs>, Vault<Arc<InMemoryFs>>) {
    let fs = Arc::new(InMemoryFs::new());
    let vault = Vault::init(Arc::clone(&fs), AUTHOR).await.unwrap();
    (fs, vault)
}

/// The UUID a path currently resolves to in the catalog (via the Index node's meta).
fn uuid_at(vault: &Vault<Arc<InMemoryFs>>, path: &str) -> Uuid {
    let node = vault
        .index()
        .node_for_path(path)
        .unwrap_or_else(|| panic!("no index node for {path}"));
    vault
        .index()
        .node_uuid(&node)
        .unwrap_or_else(|| panic!("node for {path} carries no UUID"))
}

mod on_file_changed_create {
    use super::*;

    /// Writing a new `.md` and signalling Flow-1 documents it into the catalog: a
    /// node exists with a minted UUID, the content `.loro` is written at
    /// `docs/<uuid>.loro`, and the node carries a `content_version` fingerprint.
    #[tokio::test]
    async fn creates_node_writes_uuid_loro_and_sets_content_version() {
        let (fs, vault) = empty_vault().await;

        fs.write("notes/topic.md", b"# Title\n\nBody text.")
            .await
            .unwrap();
        let changed = vault.on_file_changed("notes/topic.md").await.unwrap();
        assert!(changed, "creating a brand-new document reports a change");

        // A node exists, carrying a parseable UUID identity.
        let node = vault
            .index()
            .node_for_path("notes/topic.md")
            .expect("a node was registered for the new file");
        let uuid = vault
            .index()
            .node_uuid(&node)
            .expect("the node carries a UUID");

        // The content `.loro` was written under that UUID — NOT under any path hash.
        assert!(
            fs.exists(&content_doc_path(&uuid)).await.unwrap(),
            "the content doc was written at docs/<uuid>.loro"
        );

        // The denormalized content_version fingerprint was set at registration.
        assert!(
            vault.index().node_content_version(&node).is_some(),
            "the node carries an initial content_version fingerprint"
        );
    }

    /// The content file's name derives from the UUID, never the path: there is no
    /// path-hash `.loro` on disk, only `docs/<uuid>.loro`. This pins the "filename
    /// derives from the UUID" contract that makes a move zero-content.
    #[tokio::test]
    async fn content_loro_is_named_by_uuid_not_path() {
        let (fs, vault) = empty_vault().await;

        fs.write("a/b/note.md", b"content").await.unwrap();
        vault.on_file_changed("a/b/note.md").await.unwrap();

        let uuid = uuid_at(&vault, "a/b/note.md");
        // The one and only content file for this doc is the UUID-addressed one.
        assert!(fs.exists(&content_doc_path(&uuid)).await.unwrap());
        assert_eq!(
            content_doc_path(&uuid),
            format!(".sync/docs/{}.loro", uuid),
            "the content `.loro` is addressed purely by UUID"
        );
    }
}

mod on_file_changed_edit {
    use super::*;

    /// Editing an existing `.md` keeps the SAME UUID (identity is stable across an
    /// edit), merges the new content into the existing doc, and bumps the node's
    /// `content_version` fingerprint (the derived digest cache follows the edit).
    #[tokio::test]
    async fn edit_keeps_uuid_merges_content_and_bumps_content_version() {
        let (fs, vault) = empty_vault().await;

        fs.write("note.md", b"original body").await.unwrap();
        vault.on_file_changed("note.md").await.unwrap();

        let uuid_before = uuid_at(&vault, "note.md");
        let node = vault.index().node_for_path("note.md").unwrap();
        let version_before = vault
            .index()
            .node_content_version(&node)
            .expect("content_version set at create");

        // Edit the file on disk and re-signal Flow-1.
        fs.write("note.md", b"edited body").await.unwrap();
        let changed = vault.on_file_changed("note.md").await.unwrap();
        assert!(changed, "a real edit reports a change");

        // Same UUID — an edit does not re-mint identity.
        assert_eq!(
            uuid_at(&vault, "note.md"),
            uuid_before,
            "editing a document keeps its UUID"
        );

        // The edit merged into the doc.
        let doc = vault.get_document("note.md").await.unwrap();
        assert!(
            doc.to_markdown().contains("edited body"),
            "the new content merged into the existing document"
        );

        // The content_version fingerprint was bumped (the VV changed, so its
        // fingerprint must too).
        let version_after = vault
            .index()
            .node_content_version(&node)
            .expect("content_version still set after edit");
        assert_ne!(
            version_after, version_before,
            "a real edit bumps the node's content_version fingerprint"
        );
    }

    /// A no-op edit — rewriting byte-identical content — is echo-safe: Flow-1
    /// returns `false` and does NOT rewrite the content `.loro` (its bytes are
    /// unchanged). This is the sync-echo guard: a watcher event for content we
    /// already hold must not re-broadcast.
    #[tokio::test]
    async fn no_op_edit_returns_false_and_does_not_rewrite_loro() {
        let (fs, vault) = empty_vault().await;

        fs.write("note.md", b"# Heading\n\nStable body.")
            .await
            .unwrap();
        vault.on_file_changed("note.md").await.unwrap();

        let uuid = uuid_at(&vault, "note.md");
        let loro_path = content_doc_path(&uuid);
        let loro_before = fs.read(&loro_path).await.unwrap();

        // Re-write the identical content and re-signal Flow-1.
        fs.write("note.md", b"# Heading\n\nStable body.")
            .await
            .unwrap();
        let changed = vault.on_file_changed("note.md").await.unwrap();

        assert!(!changed, "a no-op edit reports no change (echo-safe)");
        let loro_after = fs.read(&loro_path).await.unwrap();
        assert_eq!(
            loro_before, loro_after,
            "a no-op edit must not rewrite the content `.loro`"
        );
    }

    /// A cold-cache edit (the doc isn't in memory, but its `<uuid>.loro` is on disk —
    /// e.g. after a restart) still diff-merges into the persisted doc and keeps the
    /// UUID, rather than minting a new identity. Exercises the on-disk resolution
    /// branch of Flow-1.
    #[tokio::test]
    async fn cold_cache_edit_loads_from_loro_and_keeps_uuid() {
        let fs = Arc::new(InMemoryFs::new());
        let uuid;
        {
            let vault = Vault::init(Arc::clone(&fs), AUTHOR).await.unwrap();
            fs.write("note.md", b"first").await.unwrap();
            vault.on_file_changed("note.md").await.unwrap();
            // `on_file_changed` mutates the catalog in memory; the caller flushes it
            // (the daemon does this after each watcher event). Persist so the node
            // survives the reload below.
            vault.save_index().await.unwrap();
            uuid = uuid_at(&vault, "note.md");
        }

        // Reload the vault (drops the in-memory document cache) — the catalog and
        // the `<uuid>.loro` survive on disk.
        let vault = Vault::load(Arc::clone(&fs), AUTHOR).await.unwrap();
        assert_eq!(
            uuid_at(&vault, "note.md"),
            uuid,
            "the UUID survives a reload"
        );

        // Edit with the cache cold: the doc must load from its `<uuid>.loro` and
        // merge, not mint a new doc.
        fs.write("note.md", b"second").await.unwrap();
        let changed = vault.on_file_changed("note.md").await.unwrap();
        assert!(changed, "the cold-cache edit reports a change");
        assert_eq!(
            uuid_at(&vault, "note.md"),
            uuid,
            "a cold-cache edit keeps the existing UUID (no re-mint)"
        );
        let doc = vault.get_document("note.md").await.unwrap();
        assert!(
            doc.to_markdown().contains("second"),
            "the cold-cache edit merged into the persisted document"
        );
    }
}

mod move_node {
    use super::*;

    /// Moving a document via the Index `move_node` API is pure-structural: the UUID
    /// is unchanged, the content `.loro` filename is unchanged (it was never path
    /// derived), and its bytes are byte-identical — zero content was rewritten
    /// (INV-1).
    #[tokio::test]
    async fn move_keeps_uuid_and_loro_filename_and_rewrites_zero_content() {
        let (fs, vault) = empty_vault().await;

        fs.write("inbox/draft.md", b"# Draft\n\nSome prose.")
            .await
            .unwrap();
        vault.on_file_changed("inbox/draft.md").await.unwrap();

        let uuid_before = uuid_at(&vault, "inbox/draft.md");
        let loro_path = content_doc_path(&uuid_before);
        let loro_bytes_before = fs.read(&loro_path).await.unwrap();

        // Move the node in the catalog (a structural tree op — no content touched).
        vault
            .index()
            .move_node("inbox/draft.md", "archive/draft.md")
            .unwrap();

        // The node now resolves at its new path with the SAME UUID.
        assert!(
            vault.index().node_for_path("inbox/draft.md").is_none(),
            "the old path no longer resolves"
        );
        let uuid_after = uuid_at(&vault, "archive/draft.md");
        assert_eq!(
            uuid_after, uuid_before,
            "a move keeps the document's UUID identity"
        );

        // The content `.loro` filename derives from the (unchanged) UUID, so it is
        // the same file — and its bytes were not rewritten.
        assert_eq!(
            content_doc_path(&uuid_after),
            loro_path,
            "the content `.loro` filename is unchanged by a move (UUID-addressed)"
        );
        let loro_bytes_after = fs.read(&loro_path).await.unwrap();
        assert_eq!(
            loro_bytes_before, loro_bytes_after,
            "a move rewrites zero content bytes (INV-1)"
        );
    }
}

mod init_load_round_trip {
    use super::*;
    use vault_sync::IndexError;

    /// `init` then `load` round-trips the catalog against `InMemoryFs`: a document
    /// created before the reload resolves to the same UUID after it (the persisted
    /// index + `<uuid>.loro` carry the identity across the restart).
    #[tokio::test]
    async fn init_then_load_preserves_documents_and_uuids() {
        let fs = Arc::new(InMemoryFs::new());
        let uuid;
        let vault_id;
        {
            let vault = Vault::init(Arc::clone(&fs), AUTHOR).await.unwrap();
            fs.write("dir/kept.md", b"keep me").await.unwrap();
            vault.on_file_changed("dir/kept.md").await.unwrap();
            // Flush the catalog (the caller's responsibility — `on_file_changed` is
            // flush-deferred) so the registration survives the reload.
            vault.save_index().await.unwrap();
            uuid = uuid_at(&vault, "dir/kept.md");
            vault_id = vault.vault_id();
        }

        let reloaded = Vault::load(Arc::clone(&fs), AUTHOR).await.unwrap();
        // The VaultId is stable across a reload (read back from metadata.toml).
        assert_eq!(
            reloaded.vault_id(),
            vault_id,
            "the VaultId survives an init→load round-trip"
        );
        assert_eq!(
            uuid_at(&reloaded, "dir/kept.md"),
            uuid,
            "a document's UUID survives an init→load round-trip"
        );
    }

    /// Loading a vault whose index CRDT is corrupt HARD-FAILS with a clean error
    /// (EC-9). It must NOT silently fall back to an empty index — that would
    /// re-index every file with fresh UUIDs and diverge from peers.
    #[tokio::test]
    async fn corrupt_index_on_load_hard_fails_not_empty_fallback() {
        let fs = Arc::new(InMemoryFs::new());
        {
            let vault = Vault::init(Arc::clone(&fs), AUTHOR).await.unwrap();
            fs.write("note.md", b"body").await.unwrap();
            vault.on_file_changed("note.md").await.unwrap();
        }

        // Corrupt the persisted index CRDT with bytes that are not a valid Loro doc.
        fs.write(".sync/index.loro", b"this is not a loro snapshot")
            .await
            .unwrap();

        let result = Vault::load(Arc::clone(&fs), AUTHOR).await;
        assert!(
            matches!(result, Err(IndexError::CorruptIndex(_))),
            "a corrupt index must hard-fail with CorruptIndex, got {:?}",
            result.err()
        );
    }
}

mod delete_file {
    use super::*;

    /// Create a `.md` document and return the UUID it was minted under.
    async fn seed_doc(fs: &Arc<InMemoryFs>, vault: &Vault<Arc<InMemoryFs>>, path: &str) -> Uuid {
        fs.write(path, b"some body").await.unwrap();
        vault.on_file_changed(path).await.unwrap();
        uuid_at(vault, path)
    }

    /// Deleting a live document tombstones its node, removes the content `.loro`,
    /// arms the resurrection guard, and reports `true` (so the daemon broadcasts).
    #[tokio::test]
    async fn deletes_live_document_tombstones_and_cleans_loro() {
        let (fs, vault) = empty_vault().await;
        let uuid = seed_doc(&fs, &vault, "notes/topic.md").await;
        assert!(
            fs.exists(&content_doc_path(&uuid)).await.unwrap(),
            "precondition: the content .loro exists before the delete"
        );

        let removed = vault.delete_file("notes/topic.md").await.unwrap();

        assert!(removed, "deleting a live document returns true");
        assert!(
            vault.index().node_for_path("notes/topic.md").is_none(),
            "the index node is gone after the delete"
        );
        assert!(
            vault.index().is_path_deleted("notes/topic.md"),
            "the deleted-path resurrection guard is armed after the delete"
        );
        assert!(
            !fs.exists(&content_doc_path(&uuid)).await.unwrap(),
            "the content .loro is reclaimed (no leaked docs/<uuid>.loro)"
        );
    }

    /// A second delete of the same path is an idempotent no-op: it returns `false`
    /// (the daemon must NOT re-broadcast) and never panics. The already-gone `.loro`
    /// stays gone — a no-op delete must not touch the filesystem.
    #[tokio::test]
    async fn is_idempotent_second_delete_returns_false() {
        let (fs, vault) = empty_vault().await;
        let uuid = seed_doc(&fs, &vault, "note.md").await;

        assert!(vault.delete_file("note.md").await.unwrap());

        let second = vault.delete_file("note.md").await.unwrap();
        assert!(
            !second,
            "a redundant delete of an already-tombstoned path is false"
        );
        assert!(
            !fs.exists(&content_doc_path(&uuid)).await.unwrap(),
            "the .loro stays gone across the redundant delete"
        );
    }

    /// Deleting a path that was never registered returns `false` and records no
    /// tombstone (nothing to propagate) — it warns but does not error or panic.
    #[tokio::test]
    async fn never_registered_path_returns_false() {
        let (_fs, vault) = empty_vault().await;

        let removed = vault.delete_file("ghost.md").await.unwrap();

        assert!(!removed, "deleting a never-registered path returns false");
        assert!(
            !vault.index().is_path_deleted("ghost.md"),
            "no tombstone is recorded for a genuinely unknown path"
        );
    }

    /// The deleted node no longer appears in the catalog the compare digest reads:
    /// after a delete the catalog_digest changes (the tombstone is reflected) and
    /// the path resolves to no node.
    #[tokio::test]
    async fn deleted_document_drops_out_of_the_catalog() {
        let (fs, vault) = empty_vault().await;
        seed_doc(&fs, &vault, "dir/gone.md").await;
        let digest_before = vault.catalog_digest();

        vault.delete_file("dir/gone.md").await.unwrap();

        assert!(
            vault.index().node_for_path("dir/gone.md").is_none(),
            "the deleted path resolves to no node"
        );
        assert_ne!(
            digest_before,
            vault.catalog_digest(),
            "the catalog digest reflects the tombstone (the delete changed catalog truth)"
        );
    }

    /// The tombstone is persisted immediately, so it survives a restart before any
    /// inbound sync records it. Reload a fresh handle on the same `InMemoryFs` and
    /// the path is still guarded (proving `delete_file` flushed the index, and
    /// `rebuild_caches` re-derived the guard from persisted truth).
    #[tokio::test]
    async fn tombstone_survives_reload() {
        let fs = Arc::new(InMemoryFs::new());
        {
            let vault = Vault::init(Arc::clone(&fs), AUTHOR).await.unwrap();
            fs.write("dir/doomed.md", b"body").await.unwrap();
            vault.on_file_changed("dir/doomed.md").await.unwrap();
            vault.delete_file("dir/doomed.md").await.unwrap();
        }

        let reloaded = Vault::load(Arc::clone(&fs), AUTHOR).await.unwrap();
        assert!(
            reloaded.index().is_path_deleted("dir/doomed.md"),
            "the tombstone survives the reload — delete_file flushed the index"
        );
        assert!(
            reloaded.index().node_for_path("dir/doomed.md").is_none(),
            "the deleted node does not reappear after the reload"
        );
    }

    /// Risk #1 leak-guard: the content `.loro` is addressed by the document's UUID,
    /// which `delete_file` MUST resolve BEFORE `delete_node` strips the uuid cache.
    /// Capturing the UUID up front and asserting `docs/<uuid>.loro` is gone pins the
    /// ordering: a resolve-AFTER-tombstone regression cannot find the UUID, leaks the
    /// `.loro`, and fails this assertion.
    #[tokio::test]
    async fn resolves_uuid_before_tombstone_so_loro_does_not_leak() {
        let (fs, vault) = empty_vault().await;
        let uuid = seed_doc(&fs, &vault, "leak/check.md").await;
        let loro_path = content_doc_path(&uuid);
        assert!(
            fs.exists(&loro_path).await.unwrap(),
            "precondition: the content .loro for the captured UUID exists"
        );

        vault.delete_file("leak/check.md").await.unwrap();

        assert!(
            !fs.exists(&loro_path).await.unwrap(),
            "docs/<uuid>.loro is reclaimed — proves the UUID was resolved before the \
             tombstone stripped the uuid cache (Risk #1)"
        );
    }
}
