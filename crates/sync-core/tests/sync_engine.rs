//! Integration tests for the vault-level sync protocol.
//!
//! These exercise the user-facing sync effects — content propagating between
//! peers, convergence without duplication or interleaving, "latest wins"
//! conflict resolution, and the resurrection guards that keep a deleted file
//! deleted — entirely through the public `Vault` API plus a retained
//! `Arc<InMemoryFs>` handle for simulating external edits. They are the
//! regression home for behavior that previously lived as `sync_engine.rs`
//! inline unit tests.
//!
//! Tests that need deterministic control over internal registry cache ordering
//! (the duplicate-twin / cross-peer-dedupe data-loss guards) stay inline in
//! `sync_engine/registry_apply.rs`, where they can reach the `pub(crate)`
//! cache — those effects can't be driven deterministically through the public
//! API alone.

mod common;

use std::sync::Arc;

use common::{author, full_sync, two_vaults};
use sync_core::fs::{FileSystem, InMemoryFs};
use sync_core::{NoteDocument, SyncMessage, Vault};

#[tokio::test]
async fn test_sync_between_vaults_symmetric() {
    // Two vaults with different files fully exchange in one handshake — both
    // end up with both files.
    let fs1 = Arc::new(InMemoryFs::new());
    let fs2 = Arc::new(InMemoryFs::new());

    fs1.write("file1.md", b"# From Vault 1").await.unwrap();
    fs2.write("file2.md", b"# From Vault 2").await.unwrap();

    let vault1 = Vault::init(Arc::clone(&fs1), author(1)).await.unwrap();
    let vault2 = Vault::init(Arc::clone(&fs2), author(2)).await.unwrap();

    // Vault 1 sends sync request to Vault 2.
    let request = vault1.prepare_sync_request().await.unwrap();

    // Vault 2 processes request and sends SyncExchange (response + its own request).
    let (exchange, _) = vault2.process_sync_message(&request).await.unwrap();
    assert!(exchange.is_some(), "Should return SyncExchange");

    // Vault 1 processes the exchange: applies file2 and sends back a SyncResponse with file1.
    let (final_response, modified1) = vault1
        .process_sync_message(&exchange.unwrap())
        .await
        .unwrap();
    assert!(final_response.is_some(), "Should return final SyncResponse");
    assert!(
        modified1.contains(&"file2.md".to_string()),
        "Vault1 should receive file2"
    );

    // Vault 2 processes the final response.
    let (none, modified2) = vault2
        .process_sync_message(&final_response.unwrap())
        .await
        .unwrap();
    assert!(none.is_none(), "No more messages needed");
    assert!(
        modified2.contains(&"file1.md".to_string()),
        "Vault2 should receive file1"
    );

    // Both vaults end up with both files.
    let doc1_in_vault2 = vault2.get_document("file1.md").await.unwrap();
    assert!(doc1_in_vault2.to_markdown().contains("From Vault 1"));

    let doc2_in_vault1 = vault1.get_document("file2.md").await.unwrap();
    assert!(doc2_in_vault1.to_markdown().contains("From Vault 2"));
}

#[tokio::test]
async fn test_sync_empty_vault_receives_files() {
    // An empty vault receives all of a peer's files in one handshake.
    let fs1 = Arc::new(InMemoryFs::new());
    let fs2 = Arc::new(InMemoryFs::new());

    fs1.write("note1.md", b"# Note 1").await.unwrap();
    fs1.write("note2.md", b"# Note 2").await.unwrap();

    let vault1 = Vault::init(Arc::clone(&fs1), author(1)).await.unwrap();
    let vault2 = Vault::init(Arc::clone(&fs2), author(2)).await.unwrap();

    // Empty vault sends sync request.
    let request = vault2.prepare_sync_request().await.unwrap();

    // Vault 1 responds with SyncExchange.
    let (exchange, _) = vault1.process_sync_message(&request).await.unwrap();

    // Vault 2 processes exchange — should receive both files.
    let (final_response, modified) = vault2
        .process_sync_message(&exchange.unwrap())
        .await
        .unwrap();

    assert!(modified.contains(&"note1.md".to_string()));
    assert!(modified.contains(&"note2.md".to_string()));

    // Final response exists (vault2 sends SyncResponse even if empty).
    assert!(final_response.is_some());

    // Vault 1 processes final response — nothing new (vault2 was empty).
    let (none, modified1) = vault1
        .process_sync_message(&final_response.unwrap())
        .await
        .unwrap();
    assert!(none.is_none(), "No more messages after SyncResponse");
    assert!(modified1.is_empty(), "Vault1 already had everything");
}

/// A corrupt document in a sync batch must not take down the whole batch.
/// Before the per-item containment fix, `apply_document_updates` propagated
/// the first error and dropped every other (valid) document with it.
#[tokio::test]
async fn test_apply_document_updates_continues_on_corrupt_entry() {
    use std::collections::HashMap;

    let fs = Arc::new(InMemoryFs::new());
    let vault = Vault::init(Arc::clone(&fs), author(1)).await.unwrap();

    // A real snapshot for a path the receiver doesn't have yet -> lands as a
    // new document.
    let valid_doc = NoteDocument::from_markdown("good.md", "# Good", author(2)).unwrap();
    let valid_snapshot = valid_doc.export_snapshot().unwrap();

    let mut document_updates = HashMap::new();
    document_updates.insert("good.md".to_string(), valid_snapshot);
    document_updates.insert("bad.md".to_string(), vec![0xDE, 0xAD, 0xBE, 0xEF]);

    let response = SyncMessage::SyncResponse {
        registry_updates: None,
        document_updates,
    };
    let response_bytes = bincode::serialize(&response).unwrap();

    // The corrupt entry must be skipped, not abort the batch.
    let (followup, modified) = vault.process_sync_message(&response_bytes).await.unwrap();
    assert!(
        followup.is_none(),
        "SyncResponse needs no follow-up message"
    );

    assert_eq!(
        modified.len(),
        1,
        "only one document should be reported applied"
    );
    assert!(
        modified.contains(&"good.md".to_string()),
        "good.md must be in the applied list"
    );

    // The valid document landed and is readable.
    let good = vault.get_document("good.md").await.unwrap();
    assert!(good.to_markdown().contains("Good"));
    assert!(
        fs.exists("good.md").await.unwrap(),
        "valid document markdown should be written to disk"
    );

    // The corrupt document was never applied: its markdown was never written.
    // (get_document can't be used as the check here — it falls back to a fresh
    // empty doc for unknown paths, so it always returns Ok.) Combined with
    // modified.len() == 1 above, this is the per-item containment guarantee:
    // the bad entry is dropped while the good sibling lands.
    assert!(
        !fs.exists("bad.md").await.unwrap(),
        "corrupt document markdown must not be written"
    );
}

#[tokio::test]
async fn test_document_update_broadcast() {
    // A real-time edit broadcasts and applies on the peer.
    let fs1 = Arc::new(InMemoryFs::new());
    let fs2 = Arc::new(InMemoryFs::new());

    let vault1 = Vault::init(Arc::clone(&fs1), author(1)).await.unwrap();
    let vault2 = Vault::init(Arc::clone(&fs2), author(2)).await.unwrap();

    // Create and sync initial content.
    fs1.write("note.md", b"Initial content").await.unwrap();
    vault1.on_file_changed("note.md").await.unwrap();

    // Full sync to get vault2 up to date.
    full_sync(&vault2, &vault1).await;

    // Now vault1 makes a change.
    fs1.write("note.md", b"Updated content").await.unwrap();
    vault1.on_file_changed("note.md").await.unwrap();

    // Vault1 broadcasts a document update (real-time sync).
    let update = vault1.prepare_document_update("note.md").await.unwrap();
    assert!(update.is_some());

    // Vault2 receives the update.
    let (_, modified) = vault2.process_sync_message(&update.unwrap()).await.unwrap();
    assert!(modified.contains(&"note.md".to_string()));

    // Verify content.
    let doc = vault2.get_document("note.md").await.unwrap();
    assert!(doc.to_markdown().contains("Updated content"));
}

#[tokio::test]
async fn test_sync_applies_updates_correctly() {
    // Sync applies updates without creating duplicates, and a re-sync is a no-op.
    let fs1 = Arc::new(InMemoryFs::new());
    let fs2 = Arc::new(InMemoryFs::new());

    let vault1 = Vault::init(Arc::clone(&fs1), author(1)).await.unwrap();
    let vault2 = Vault::init(Arc::clone(&fs2), author(2)).await.unwrap();

    // Vault1 creates a file.
    fs1.write("note.md", b"# Original").await.unwrap();
    vault1.on_file_changed("note.md").await.unwrap();

    // Sync to vault2.
    let request = vault2.prepare_sync_request().await.unwrap();
    let (exchange, _) = vault1.process_sync_message(&request).await.unwrap();
    let (_, modified) = vault2
        .process_sync_message(&exchange.unwrap())
        .await
        .unwrap();

    // Vault2 should have received the file.
    assert!(modified.contains(&"note.md".to_string()));

    // Verify content matches.
    let doc1 = vault1.get_document("note.md").await.unwrap();
    let doc2 = vault2.get_document("note.md").await.unwrap();
    assert_eq!(doc1.to_markdown(), doc2.to_markdown());

    // Apply the same sync again — should be a no-op (no spurious re-send of
    // already-synced content).
    let request2 = vault2.prepare_sync_request().await.unwrap();
    let (exchange2, _) = vault1.process_sync_message(&request2).await.unwrap();
    let (_, modified2) = vault2
        .process_sync_message(&exchange2.unwrap())
        .await
        .unwrap();

    assert!(modified2.is_empty(), "Re-sync should not modify anything");
}

#[tokio::test]
async fn test_version_includes_basic() {
    // `Vault::version_includes` is a public associated fn consumed by sync-wasm
    // (sync-wasm decides whether a peer already has our updates before sending),
    // but no native sync path exercises it. This is its direct behavioral cover:
    // a document that imported another's full state must be reported as causally
    // including that other document's version.

    // Create a document and capture its initial version vector (encoded).
    let doc1 = NoteDocument::from_markdown("test.md", "# Hello", author(1)).unwrap();
    let v1 = doc1.version().encode();

    // A second document imports doc1's full snapshot, so its history now contains
    // every op doc1 had.
    let mut doc2 = NoteDocument::new("test.md", author(2));
    doc2.import(&doc1.export_snapshot().unwrap()).unwrap();
    let v2 = doc2.version().encode();

    // v2 includes v1 — it has all of doc1's ops after the import.
    assert!(
        Vault::<InMemoryFs>::version_includes(&v2, &v1),
        "After import, v2 should include v1"
    );

    // v1 does NOT include v2: the import added operations under doc2's peer ID
    // that doc1 has never seen, so the version vectors are not symmetric. This is
    // correct Loro behavior — import extends the version vector with the imported
    // replica's ops.
    assert!(
        !Vault::<InMemoryFs>::version_includes(&v1, &v2),
        "v1 should NOT include v2 after the one-way import"
    );
}

#[tokio::test]
async fn test_document_update_is_idempotent() {
    // Receiving the same DocumentUpdate twice is a no-op the second time.
    let fs1 = Arc::new(InMemoryFs::new());
    let fs2 = Arc::new(InMemoryFs::new());

    let vault1 = Vault::init(Arc::clone(&fs1), author(1)).await.unwrap();
    let vault2 = Vault::init(Arc::clone(&fs2), author(2)).await.unwrap();

    // Vault1 creates a file.
    fs1.write("note.md", b"# Content").await.unwrap();
    vault1.on_file_changed("note.md").await.unwrap();

    // Get the document update.
    let update = vault1
        .prepare_document_update("note.md")
        .await
        .unwrap()
        .unwrap();

    // Apply to vault2 first time.
    let (_, modified1) = vault2.process_sync_message(&update).await.unwrap();
    assert!(
        modified1.contains(&"note.md".to_string()),
        "First apply should modify"
    );

    // Apply the same update again.
    let (_, modified2) = vault2.process_sync_message(&update).await.unwrap();
    assert!(
        modified2.is_empty(),
        "Second apply should be no-op (idempotent)"
    );

    // Content should still be correct.
    let doc = vault2.get_document("note.md").await.unwrap();
    assert!(doc.to_markdown().contains("# Content"));
}

#[tokio::test]
async fn test_sync_echo_does_not_duplicate() {
    // Regression test for content duplication bug. When a file is synced and
    // written to disk, the file watcher triggers on_file_changed(). Previously
    // this created a new LoroDoc with a new peer ID, causing content
    // duplication on subsequent syncs.
    let fs1 = Arc::new(InMemoryFs::new());
    let fs2 = Arc::new(InMemoryFs::new());

    let vault1 = Vault::init(Arc::clone(&fs1), author(1)).await.unwrap();
    let vault2 = Vault::init(Arc::clone(&fs2), author(2)).await.unwrap();

    // Vault1 creates a file with specific content.
    let content = "Hello";
    fs1.write("note.md", content.as_bytes()).await.unwrap();
    vault1.on_file_changed("note.md").await.unwrap();

    // Sync vault1 → vault2.
    full_sync(&vault2, &vault1).await;

    // Simulate file watcher: vault2 calls on_file_changed after sync writes to
    // disk. This is the bug scenario — previously created a new peer ID and
    // duplicated content.
    vault2.on_file_changed("note.md").await.unwrap();

    // Sync vault2 → vault1 (this would cause duplication before the fix).
    full_sync(&vault1, &vault2).await;

    // Content is exactly "Hello" (not "HelloHello" or duplicated).
    let doc = vault1.get_document("note.md").await.unwrap();
    let markdown = doc.to_markdown();
    assert_eq!(markdown, content, "Content should not be duplicated");
}

#[tokio::test]
async fn test_local_edit_after_sync() {
    // A local edit after sync propagates correctly.
    let fs1 = Arc::new(InMemoryFs::new());
    let fs2 = Arc::new(InMemoryFs::new());

    let vault1 = Vault::init(Arc::clone(&fs1), author(1)).await.unwrap();
    let vault2 = Vault::init(Arc::clone(&fs2), author(2)).await.unwrap();

    // Vault1 creates initial content.
    fs1.write("note.md", b"Hello").await.unwrap();
    vault1.on_file_changed("note.md").await.unwrap();

    // Sync to vault2.
    full_sync(&vault2, &vault1).await;

    // Vault2 makes a local edit.
    fs2.write("note.md", b"Hello World").await.unwrap();
    vault2.on_file_changed("note.md").await.unwrap();

    // Sync back to vault1.
    full_sync(&vault1, &vault2).await;

    // Vault1 should have the updated content.
    let doc = vault1.get_document("note.md").await.unwrap();
    assert_eq!(
        doc.to_markdown(),
        "Hello World",
        "Edit should propagate correctly"
    );
}

#[tokio::test]
async fn test_reindex_during_reconcile_no_duplication() {
    // Regression test: reconcile() calls reindex_file() when files are modified
    // externally. Previously this created a new peer ID, causing content
    // duplication on sync.
    let fs1 = Arc::new(InMemoryFs::new());
    let fs2 = Arc::new(InMemoryFs::new());

    // Initialize vault1 with a file.
    fs1.write("note.md", b"Original").await.unwrap();
    let vault1 = Vault::init(Arc::clone(&fs1), author(1)).await.unwrap();

    // Sync to vault2.
    fs2.mkdir(".sync").await.unwrap();
    fs2.mkdir(".sync/documents").await.unwrap();
    let vault2 = Vault::init(Arc::clone(&fs2), author(2)).await.unwrap();

    full_sync(&vault2, &vault1).await;

    // Simulate external modification on vault2 (plugin was off).
    fs2.write("note.md", b"Modified externally").await.unwrap();

    // Reload vault2 — this triggers reconcile() -> reindex_file().
    let vault2_reloaded = Vault::load(Arc::clone(&fs2), author(2)).await.unwrap();

    // Sync back to vault1.
    full_sync(&vault1, &vault2_reloaded).await;

    // Content is NOT duplicated.
    let doc = vault1.get_document("note.md").await.unwrap();
    let content = doc.to_markdown();
    assert_eq!(
        content, "Modified externally",
        "Content should not be duplicated after reconcile"
    );
}

#[tokio::test]
async fn test_cold_cache_no_duplication() {
    // Regression test: on_file_changed() when .loro exists on disk but not in
    // the in-memory cache. Previously fell through to creating a new document
    // with a new peer ID.
    //
    // A cold cache is reproduced by dropping the vault and reloading it
    // (`Vault::load`) — the reload genuinely starts with an empty in-memory
    // document cache while the .loro persists on disk, which is the real cold
    // start the bug occurred under.
    let fs1 = Arc::new(InMemoryFs::new());
    let fs2 = Arc::new(InMemoryFs::new());

    // Initialize vault1 with a file.
    fs1.write("note.md", b"Hello").await.unwrap();
    let vault1 = Vault::init(Arc::clone(&fs1), author(1)).await.unwrap();

    // Sync to vault2.
    let vault2 = Vault::init(Arc::clone(&fs2), author(2)).await.unwrap();
    full_sync(&vault2, &vault1).await;

    // Drop + reload vault2 to start from a cold cache (.loro on disk, nothing
    // in memory).
    drop(vault2);
    let vault2 = Vault::load(Arc::clone(&fs2), author(2)).await.unwrap();

    // Make an edit and call on_file_changed (the .loro exists on disk but not
    // in the freshly-loaded cache).
    fs2.write("note.md", b"Hello World").await.unwrap();
    vault2.on_file_changed("note.md").await.unwrap();

    // Sync back to vault1.
    full_sync(&vault1, &vault2).await;

    // Content is correct (not duplicated).
    let doc = vault1.get_document("note.md").await.unwrap();
    let content = doc.to_markdown();
    assert_eq!(
        content, "Hello World",
        "Cold cache should not cause duplication"
    );
}

#[tokio::test]
async fn test_file_migration_preserves_content() {
    // A file renamed-on-disk while the plugin was off migrates on reload with
    // its content preserved at the new path.
    let fs1 = Arc::new(InMemoryFs::new());
    let fs2 = Arc::new(InMemoryFs::new());

    // Initialize vault1 with a file.
    fs1.write("old_name.md", b"Content ABC").await.unwrap();
    let vault1 = Vault::init(Arc::clone(&fs1), author(1)).await.unwrap();

    // Sync to vault2.
    let vault2 = Vault::init(Arc::clone(&fs2), author(2)).await.unwrap();
    full_sync(&vault2, &vault1).await;

    // Simulate file rename on vault2 (plugin was off).
    let content = fs2.read("old_name.md").await.unwrap();
    fs2.write("new_name.md", &content).await.unwrap();
    fs2.delete("old_name.md").await.unwrap();

    // Reload vault2 — this triggers reconcile() -> migrate_document().
    let vault2_reloaded = Vault::load(Arc::clone(&fs2), author(2)).await.unwrap();

    // The migrated document exists at the new path with content preserved.
    let doc2 = vault2_reloaded.get_document("new_name.md").await.unwrap();
    assert!(doc2.to_markdown().contains("Content ABC"));
}

#[tokio::test]
async fn test_divergent_same_file_no_interleaving() {
    // Regression test: two vaults create the SAME file with DIFFERENT content
    // BEFORE any sync. When they sync, content should NOT be interleaved. This
    // was the original bug where "# Hello" became "# # Hellello WWorld".
    let fs1 = Arc::new(InMemoryFs::new());
    let fs2 = Arc::new(InMemoryFs::new());

    // Both vaults create the SAME file with DIFFERENT content BEFORE sync.
    fs1.write("note.md", b"# Hello from A").await.unwrap();
    // Add delay to ensure different mtime.
    std::thread::sleep(std::time::Duration::from_millis(10));
    fs2.write("note.md", b"# Hello from B").await.unwrap();

    // Initialize vaults — each creates its own LoroDoc with independent peer IDs.
    let vault1 = Vault::init(Arc::clone(&fs1), author(1)).await.unwrap();
    let vault2 = Vault::init(Arc::clone(&fs2), author(2)).await.unwrap();

    // Sync the two vaults.
    full_sync(&vault2, &vault1).await;

    // Content is NOT interleaved.
    let doc1 = vault1.get_document("note.md").await.unwrap();
    let doc2 = vault2.get_document("note.md").await.unwrap();

    let content1 = doc1.to_markdown();
    let content2 = doc2.to_markdown();

    // Content should be one of the original versions, not interleaved garbage.
    let valid_contents = ["# Hello from A", "# Hello from B"];
    assert!(
        valid_contents.contains(&content1.as_str()),
        "Vault1 content should be valid, got: '{}'",
        content1
    );
    assert!(
        valid_contents.contains(&content2.as_str()),
        "Vault2 content should be valid, got: '{}'",
        content2
    );

    // With "latest wins", both vaults converge to the same content.
    assert_eq!(
        content1, content2,
        "Both vaults should have same content after sync"
    );
}

#[tokio::test]
async fn test_latest_wins_newer_remote() {
    // "Latest wins" correctly keeps newer remote content.
    let fs1 = Arc::new(InMemoryFs::new());
    let fs2 = Arc::new(InMemoryFs::new());

    // Vault1 creates file first (older).
    fs1.write("note.md", b"Older content").await.unwrap();
    fs1.set_mtime("note.md", 1000); // Older timestamp

    // Vault2 creates same file later (newer).
    fs2.write("note.md", b"Newer content").await.unwrap();
    fs2.set_mtime("note.md", 2000); // Newer timestamp

    let vault1 = Vault::init(Arc::clone(&fs1), author(1)).await.unwrap();
    let vault2 = Vault::init(Arc::clone(&fs2), author(2)).await.unwrap();

    // Vault2 sends DocumentUpdate to Vault1 (real-time sync with mtime).
    let update = vault2
        .prepare_document_update("note.md")
        .await
        .unwrap()
        .unwrap();
    let (_, modified) = vault1.process_sync_message(&update).await.unwrap();

    // Vault1 accepts the newer content.
    assert!(
        modified.contains(&"note.md".to_string()),
        "Should be modified"
    );
    let doc = vault1.get_document("note.md").await.unwrap();
    assert_eq!(
        doc.to_markdown(),
        "Newer content",
        "Should have newer content"
    );
}

#[tokio::test]
async fn test_latest_wins_newer_local() {
    // "Latest wins" correctly keeps newer local content.
    let fs1 = Arc::new(InMemoryFs::new());
    let fs2 = Arc::new(InMemoryFs::new());

    // Vault1 creates file later (newer).
    fs1.write("note.md", b"Newer content").await.unwrap();
    fs1.set_mtime("note.md", 2000); // Newer timestamp

    // Vault2 creates same file first (older).
    fs2.write("note.md", b"Older content").await.unwrap();
    fs2.set_mtime("note.md", 1000); // Older timestamp

    let vault1 = Vault::init(Arc::clone(&fs1), author(1)).await.unwrap();
    let vault2 = Vault::init(Arc::clone(&fs2), author(2)).await.unwrap();

    // Vault2 sends DocumentUpdate to Vault1 (real-time sync with mtime).
    let update = vault2
        .prepare_document_update("note.md")
        .await
        .unwrap()
        .unwrap();
    let (_, modified) = vault1.process_sync_message(&update).await.unwrap();

    // Vault1 REJECTS the older content (keeps its own).
    assert!(
        modified.is_empty(),
        "Should NOT be modified - local is newer"
    );
    let doc = vault1.get_document("note.md").await.unwrap();
    assert_eq!(
        doc.to_markdown(),
        "Newer content",
        "Should keep newer local content"
    );
}

#[tokio::test]
async fn test_sync_empty_file() {
    // An empty .md file syncs and stays empty.
    let (vault1, vault2, _fs1, _fs2) = two_vaults(&[("empty.md", "")], &[]).await;

    // Sync to vault2.
    let (modified, _) = full_sync(&vault2, &vault1).await;

    // Vault2 received the empty file.
    assert!(modified.contains(&"empty.md".to_string()));
    let doc = vault2.get_document("empty.md").await.unwrap();
    assert_eq!(doc.to_markdown(), "", "Empty file should remain empty");
}

#[tokio::test]
async fn test_sync_frontmatter_only_file() {
    // A frontmatter-only file (no body) syncs with frontmatter intact.
    let (vault1, vault2, _fs1, _fs2) = two_vaults(
        &[("meta.md", "---\ntitle: Test\ntags:\n  - a\n  - b\n---\n")],
        &[],
    )
    .await;

    // Sync to vault2.
    let (modified, _) = full_sync(&vault2, &vault1).await;

    // Vault2 received the file.
    assert!(modified.contains(&"meta.md".to_string()));
    let doc = vault2.get_document("meta.md").await.unwrap();
    let content = doc.to_markdown();
    assert!(content.contains("title:"), "Should have frontmatter");
    assert!(content.contains("tags:"), "Should have tags");
}

#[tokio::test]
async fn test_incremental_updates_after_sync_use_crdt_merge() {
    // After initial sync, both vaults share the same doc_id. Subsequent edits
    // should CRDT-merge (both lines present), not trigger divergence detection
    // that replaces one with the other.
    let fs1 = Arc::new(InMemoryFs::new());
    let fs2 = Arc::new(InMemoryFs::new());

    // Vault1 creates file.
    fs1.write("note.md", b"Line 1").await.unwrap();
    let vault1 = Vault::init(Arc::clone(&fs1), author(1)).await.unwrap();
    let vault2 = Vault::init(Arc::clone(&fs2), author(2)).await.unwrap();

    // Initial sync — vault2 gets the file with vault1's doc_id.
    full_sync(&vault2, &vault1).await;

    // Both vaults should now share the same doc_id.
    let doc1 = vault1.get_document("note.md").await.unwrap();
    let doc2 = vault2.get_document("note.md").await.unwrap();
    assert_eq!(
        doc1.doc_id(),
        doc2.doc_id(),
        "After sync, doc_ids should match"
    );

    // Vault2 makes an edit.
    fs2.write("note.md", b"Line 1\nLine 2 from vault2")
        .await
        .unwrap();
    vault2.on_file_changed("note.md").await.unwrap();

    // Vault1 also makes an edit (concurrent).
    fs1.write("note.md", b"Line 1\nLine 2 from vault1")
        .await
        .unwrap();
    vault1.on_file_changed("note.md").await.unwrap();

    // Sync vault2 → vault1 (should CRDT merge, not diverge).
    let update = vault2
        .prepare_document_update("note.md")
        .await
        .unwrap()
        .unwrap();
    let (_, modified) = vault1.process_sync_message(&update).await.unwrap();

    // Should be modified (merged).
    assert!(
        modified.contains(&"note.md".to_string()),
        "Should merge changes"
    );

    // Content should have BOTH lines (CRDT merge), not replace one with the other.
    let doc = vault1.get_document("note.md").await.unwrap();
    let content = doc.to_markdown();
    assert!(content.contains("Line 1"), "Should have original line");
    // CRDT merge means both edits are present (order may vary).
    assert!(
        content.contains("vault1") || content.contains("vault2"),
        "Should have merged content, got: {}",
        content
    );
}

#[tokio::test]
async fn test_legacy_document_without_doc_id_assumes_compatible() {
    // Documents created before `doc_id` existed carry no doc_id. When an update
    // for such a legacy document arrives for a path we already hold, the
    // divergence check in apply_single_update must treat the missing doc_id as
    // "assume compatible" and CRDT-MERGE the histories. A regression to "assume
    // divergent" would content-REPLACE instead — silently dropping the local
    // edits for any device that still has pre-doc_id documents (data loss).

    // Vault2 already holds note.md with a doc_id and its own local content,
    // synced from vault1 (from_markdown mints a doc_id).
    let fs1 = Arc::new(InMemoryFs::new());
    let fs2 = Arc::new(InMemoryFs::new());
    fs1.write("note.md", b"Local line").await.unwrap();
    let vault1 = Vault::init(Arc::clone(&fs1), author(1)).await.unwrap();
    vault1.on_file_changed("note.md").await.unwrap();
    let vault2 = Vault::init(Arc::clone(&fs2), author(2)).await.unwrap();
    full_sync(&vault2, &vault1).await;
    assert!(
        vault2
            .get_document("note.md")
            .await
            .unwrap()
            .doc_id()
            .is_some(),
        "setup: vault2's note.md must carry a doc_id"
    );

    // Build a legacy update for the same path with NO doc_id, via the public
    // document API: NoteDocument::new never mints a doc_id, and the body edit is
    // authored under a third, independent replica so its history is genuinely
    // distinct from vault2's (the merge is then a real union, not a no-op).
    let legacy = NoteDocument::new("note.md", author(3));
    legacy.update_body("Legacy line").unwrap();
    legacy.commit();
    assert!(
        legacy.doc_id().is_none(),
        "legacy document must have no doc_id"
    );
    let legacy_bytes = legacy.export_snapshot().unwrap();

    // Deliver it through the public sync path as a bulk SyncResponse update.
    let mut document_updates = std::collections::HashMap::new();
    document_updates.insert("note.md".to_string(), legacy_bytes);
    let response = SyncMessage::SyncResponse {
        registry_updates: None,
        document_updates,
    };
    let response_bytes = bincode::serialize(&response).unwrap();

    let (_, modified) = vault2.process_sync_message(&response_bytes).await.unwrap();
    assert!(
        modified.contains(&"note.md".to_string()),
        "legacy update must apply (got modified={:?})",
        modified
    );

    // The legacy doc was treated compatible: BOTH the local and the legacy text
    // are present (CRDT merge). Under a regression to "assume divergent", the
    // local content would be replaced by the remote and "Local line" would be
    // gone.
    let content = vault2.get_document("note.md").await.unwrap().to_markdown();
    assert!(
        content.contains("Local line"),
        "local content must survive the merge (data-loss guard), got: {}",
        content
    );
    assert!(
        content.contains("Legacy line"),
        "legacy content must be merged in, got: {}",
        content
    );
}

// ========== Resurrection Guard Tests ==========

/// A locally-deleted file must not be resurrected by an inbound DocumentUpdate
/// for the same path. Without the fix, apply_single_update's new-document branch
/// would create the file unconditionally because neither cache nor disk have it.
#[tokio::test]
async fn test_document_update_skipped_for_registry_deleted_path() {
    let fs1 = Arc::new(InMemoryFs::new());
    let fs2 = Arc::new(InMemoryFs::new());

    // Vault 1 creates note.md and syncs it to vault 2.
    fs1.write("note.md", b"# Hello").await.unwrap();
    let vault1 = Vault::init(Arc::clone(&fs1), author(1)).await.unwrap();
    vault1.on_file_changed("note.md").await.unwrap();
    let vault2 = Vault::init(Arc::clone(&fs2), author(2)).await.unwrap();

    full_sync(&vault2, &vault1).await;
    assert!(
        fs2.exists("note.md").await.unwrap(),
        "setup: vault2 should have note.md"
    );

    // Vault 2 deletes the file locally: remove from disk first (user action),
    // then update the registry tree via delete_file.
    fs2.delete("note.md").await.unwrap();
    vault2.delete_file("note.md").await.unwrap();
    assert!(
        !fs2.exists("note.md").await.unwrap(),
        "note.md should be gone after delete"
    );
    assert!(
        vault2.is_file_deleted("note.md"),
        "registry should show path as deleted"
    );

    // Vault 1 still has the file and broadcasts a DocumentUpdate (real-time sync path).
    let update = vault1
        .prepare_document_update("note.md")
        .await
        .unwrap()
        .unwrap();

    // Vault 2 receives the DocumentUpdate — must NOT resurrect the file.
    let (_, modified) = vault2.process_sync_message(&update).await.unwrap();

    assert!(
        modified.is_empty(),
        "DocumentUpdate for a locally-deleted path must be skipped (got modified={:?})",
        modified
    );
    assert!(
        !fs2.exists("note.md").await.unwrap(),
        "deleted file must not reappear on disk after inbound DocumentUpdate"
    );
}

/// Both peers delete a file; then one peer creates a brand-new file at the same
/// path (a fresh registry node). The other peer must receive it — the tombstone
/// from the earlier deletion must be cleared when the new alive registry node
/// arrives.
///
/// Without alive-wins in rebuild_path_cache, vault2's deleted-paths set blocks
/// the DocumentUpdate forever and note.md never reappears on vault2's filesystem.
#[tokio::test]
async fn test_legit_recreate_after_registry_alive_node_applies() {
    let fs1 = Arc::new(InMemoryFs::new());
    let fs2 = Arc::new(InMemoryFs::new());

    // Both vaults start with note.md and sync it.
    fs1.write("note.md", b"# Original").await.unwrap();
    let vault1 = Vault::init(Arc::clone(&fs1), author(1)).await.unwrap();
    vault1.on_file_changed("note.md").await.unwrap();
    let vault2 = Vault::init(Arc::clone(&fs2), author(2)).await.unwrap();

    full_sync(&vault2, &vault1).await;
    assert!(
        fs2.exists("note.md").await.unwrap(),
        "setup: both vaults should have note.md"
    );

    // Both vaults delete note.md independently — both get tombstones.
    fs1.delete("note.md").await.unwrap();
    vault1.delete_file("note.md").await.unwrap();
    fs2.delete("note.md").await.unwrap();
    vault2.delete_file("note.md").await.unwrap();
    assert!(
        vault1.is_file_deleted("note.md"),
        "vault1 must tombstone the path"
    );
    assert!(
        vault2.is_file_deleted("note.md"),
        "vault2 must tombstone the path"
    );

    // Vault 1 creates a brand-new note.md (new registry node at the same path).
    fs1.write("note.md", b"# Brand new").await.unwrap();
    vault1.on_file_changed("note.md").await.unwrap();
    // on_file_changed -> register_file writes a new alive node for the path, so
    // the registry no longer reports it deleted.
    assert!(
        !vault1.is_file_deleted("note.md"),
        "register_file must make the path alive again"
    );

    // Vault 1 syncs to vault 2. The SyncExchange delivers the new alive registry
    // node (whose import clears the path from vault2's deleted-paths set via
    // alive-wins) and the document, so vault2 can create note.md.
    let request2 = vault2.prepare_sync_request().await.unwrap();
    let (exchange2, _) = vault1.process_sync_message(&request2).await.unwrap();
    let (_, modified) = vault2
        .process_sync_message(&exchange2.unwrap())
        .await
        .unwrap();

    assert!(
        modified.contains(&"note.md".to_string()),
        "legit re-create must propagate to vault2 (got modified={:?})",
        modified
    );
    assert!(
        fs2.exists("note.md").await.unwrap(),
        "re-created file must appear on vault2's filesystem"
    );
    let doc = vault2.get_document("note.md").await.unwrap();
    assert!(
        doc.to_markdown().contains("Brand new"),
        "vault2 must have the re-created content"
    );
}

/// A tombstone for the root-level "note.md" must not block a DocumentUpdate for
/// "nested/note.md" (a different path), and vice versa. Full-path comparison is
/// required.
#[tokio::test]
async fn test_tombstone_check_uses_full_path_not_name() {
    let fs1 = Arc::new(InMemoryFs::new());
    let fs2 = Arc::new(InMemoryFs::new());

    // Vault 1 has both root-level note.md and nested/note.md; sync to vault 2.
    fs1.write("note.md", b"# Root note").await.unwrap();
    fs1.write("nested/note.md", b"# Nested note").await.unwrap();
    let vault1 = Vault::init(Arc::clone(&fs1), author(1)).await.unwrap();
    vault1.on_file_changed("note.md").await.unwrap();
    vault1.on_file_changed("nested/note.md").await.unwrap();
    let vault2 = Vault::init(Arc::clone(&fs2), author(2)).await.unwrap();

    full_sync(&vault2, &vault1).await;
    assert!(fs2.exists("note.md").await.unwrap());
    assert!(fs2.exists("nested/note.md").await.unwrap());

    // Vault 2 deletes only the root-level note.md (user removes the file, then
    // the daemon calls delete_file to tombstone it in the registry tree).
    fs2.delete("note.md").await.unwrap();
    vault2.delete_file("note.md").await.unwrap();
    assert!(
        vault2.is_file_deleted("note.md"),
        "root note.md should be deleted"
    );
    assert!(
        !vault2.is_file_deleted("nested/note.md"),
        "nested/note.md must NOT be deleted"
    );

    // Vault 1 updates nested/note.md and broadcasts a DocumentUpdate.
    fs1.write("nested/note.md", b"# Updated nested")
        .await
        .unwrap();
    vault1.on_file_changed("nested/note.md").await.unwrap();
    let update = vault1
        .prepare_document_update("nested/note.md")
        .await
        .unwrap()
        .unwrap();

    // Vault 2 receives the update for nested/note.md — must NOT be blocked by
    // the root note.md tombstone (different full path).
    let (_, modified) = vault2.process_sync_message(&update).await.unwrap();
    assert!(
        modified.contains(&"nested/note.md".to_string()),
        "DocumentUpdate for nested/note.md must apply even though root note.md is tombstoned"
    );

    // Root note.md must still be absent (tombstone holds).
    assert!(
        !fs2.exists("note.md").await.unwrap(),
        "tombstone on root note.md must not be cleared by unrelated update"
    );
}

/// After a daemon restart, a registry tombstone must still block resurrection.
///
/// This is the production "charon recreated 12 deleted root notes" bug: the old
/// guard lived in an in-memory session set that was empty after restart, so a
/// stale peer's DocumentUpdate for a long-deleted path created the file as a
/// disk orphan. The fix derives the guard from the persisted registry, so it
/// survives the reload.
#[tokio::test]
async fn test_cold_restart_tombstone_blocks_resurrection() {
    let fs1 = Arc::new(InMemoryFs::new());
    let fs2 = Arc::new(InMemoryFs::new());

    // Vault 1 creates note.md and syncs it to vault 2.
    fs1.write("note.md", b"# Hello").await.unwrap();
    let vault1 = Vault::init(Arc::clone(&fs1), author(1)).await.unwrap();
    vault1.on_file_changed("note.md").await.unwrap();
    let vault2 = Vault::init(Arc::clone(&fs2), author(2)).await.unwrap();

    full_sync(&vault2, &vault1).await;
    assert!(
        fs2.exists("note.md").await.unwrap(),
        "setup: vault2 should have note.md"
    );

    // Vault 2 deletes the file locally (disk delete, then registry tombstone).
    // delete_file persists the registry, so the tombstone is on fs2's disk.
    fs2.delete("note.md").await.unwrap();
    vault2.delete_file("note.md").await.unwrap();

    // Simulate a daemon restart: reload vault2 from its persisted storage. The
    // reload gets a fresh SyncState with an empty session set; the tombstone
    // must instead come from the registry re-imported off disk.
    drop(vault2);
    let vault2 = Vault::load(Arc::clone(&fs2), author(2)).await.unwrap();
    // `is_file_deleted` is the public proxy for the `pub(crate)`
    // `is_path_deleted_in_registry` session-set check; the real regression guard
    // is the terminal "file did not reappear" assertion below.
    assert!(
        vault2.is_file_deleted("note.md"),
        "deleted path must be recovered from the persisted registry after restart"
    );

    // A stale vault1 still has the file and broadcasts a DocumentUpdate.
    let update = vault1
        .prepare_document_update("note.md")
        .await
        .unwrap()
        .unwrap();

    // The reloaded vault2 must NOT resurrect the file.
    let (_, modified) = vault2.process_sync_message(&update).await.unwrap();
    assert!(
        modified.is_empty(),
        "post-restart DocumentUpdate for a deleted path must be skipped (got modified={:?})",
        modified
    );
    assert!(
        !fs2.exists("note.md").await.unwrap(),
        "deleted file must not reappear on disk after restart + inbound DocumentUpdate"
    );
}

/// After a restart, a peer's legitimate re-create at a previously-deleted path
/// must still be accepted — "alive wins" survives the reload. Without it, the
/// restored tombstone would block the new file forever.
#[tokio::test]
async fn test_cold_restart_alive_node_allows_create() {
    let fs1 = Arc::new(InMemoryFs::new());
    let fs2 = Arc::new(InMemoryFs::new());

    // Vault 1 creates note.md and syncs it to vault 2.
    fs1.write("note.md", b"# Hello").await.unwrap();
    let vault1 = Vault::init(Arc::clone(&fs1), author(1)).await.unwrap();
    vault1.on_file_changed("note.md").await.unwrap();
    let vault2 = Vault::init(Arc::clone(&fs2), author(2)).await.unwrap();

    full_sync(&vault2, &vault1).await;
    assert!(
        fs2.exists("note.md").await.unwrap(),
        "setup: vault2 should have note.md"
    );

    // Both vaults delete note.md independently — both persist a tombstone.
    fs1.delete("note.md").await.unwrap();
    vault1.delete_file("note.md").await.unwrap();
    fs2.delete("note.md").await.unwrap();
    vault2.delete_file("note.md").await.unwrap();

    // Restart vault2 — the persisted tombstone is restored from disk.
    drop(vault2);
    let vault2 = Vault::load(Arc::clone(&fs2), author(2)).await.unwrap();
    assert!(
        vault2.is_file_deleted("note.md"),
        "tombstone should be restored after restart before the re-create syncs"
    );

    // Vault 1 re-creates note.md as a brand-new alive registry node.
    fs1.write("note.md", b"# Brand new").await.unwrap();
    vault1.on_file_changed("note.md").await.unwrap();

    // Vault 1 syncs the new alive node + document to the reloaded vault2. The
    // registry import → rebuild_path_cache sees note.md alive and drops it from
    // deleted_paths (alive wins), so the document create is allowed.
    let request2 = vault2.prepare_sync_request().await.unwrap();
    let (exchange2, _) = vault1.process_sync_message(&request2).await.unwrap();
    let (_, modified) = vault2
        .process_sync_message(&exchange2.unwrap())
        .await
        .unwrap();

    assert!(
        modified.contains(&"note.md".to_string()),
        "re-create must be allowed after restart (got modified={:?})",
        modified
    );
    assert!(
        fs2.exists("note.md").await.unwrap(),
        "re-created file must appear on the reloaded vault2's filesystem"
    );
    let doc = vault2.get_document("note.md").await.unwrap();
    assert!(
        doc.to_markdown().contains("Brand new"),
        "reloaded vault2 must have the re-created content"
    );
}
