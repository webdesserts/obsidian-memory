//! Integration tests for vault registry-sync effects, sync flags, and the
//! debug/inspection API.
//!
//! Registry-sync effects: a deletion propagates to a peer via the full
//! handshake; a move removes the old path on the receiver (the move-strands
//! regression that stranded 25 root notes in production); a swap (two files
//! exchanging paths in one exchange) leaves both files alive on the receiver
//! (the data-loss guard). Sync flags: an inbound synced write arms the
//! echo-suppression flag so the daemon won't re-broadcast it, while a local edit
//! does not. Debug API: the inspection surface the sync-daemon and MCP layer
//! consume (`get_registry_version` / `get_registry_stats` /
//! `get_document_blob_meta` / `get_document_info`). All drive the public `Vault`
//! API plus retained `Arc<InMemoryFs>` handles.

mod common;

use std::sync::Arc;

use common::author;
use sync_core::Vault;
use sync_core::fs::{FileSystem, InMemoryFs};

// ========== Registry-sync effects ==========

#[tokio::test]
async fn delete_syncs_via_registry() {
    // A deletion in one vault propagates to a peer's alive set via the registry
    // sync handshake.
    let fs1 = Arc::new(InMemoryFs::new());
    let fs2 = Arc::new(InMemoryFs::new());

    fs1.write("note.md", b"# Hello").await.unwrap();
    let vault1 = Vault::init(Arc::clone(&fs1), author(1)).await.unwrap();

    // Sync to vault2.
    let vault2 = Vault::init(Arc::clone(&fs2), author(2)).await.unwrap();
    let request = vault2.prepare_sync_request().await.unwrap();
    let (exchange, _) = vault1.process_sync_message(&request).await.unwrap();
    let (final_resp, _) = vault2
        .process_sync_message(&exchange.unwrap())
        .await
        .unwrap();
    if let Some(resp) = final_resp {
        vault1.process_sync_message(&resp).await.unwrap();
    }

    assert!(!vault1.is_file_deleted("note.md"));
    assert!(!vault2.is_file_deleted("note.md"));

    // Delete in vault1.
    vault1.delete_file("note.md").await.unwrap();
    assert!(vault1.is_file_deleted("note.md"));

    // Sync again — vault2 should see the deletion via the registry.
    let request2 = vault2.prepare_sync_request().await.unwrap();
    let (exchange2, _) = vault1.process_sync_message(&request2).await.unwrap();
    vault2
        .process_sync_message(&exchange2.unwrap())
        .await
        .unwrap();

    assert!(vault2.is_file_deleted("note.md"));
}

/// A registry-mediated move (tree.mov, same node, new parent path) must clean up
/// the old physical `.md` on the RECEIVER. Without this, every move leaves an
/// untracked orphan at the old path on every peer — the bug that accumulated 25
/// stranded root notes in production.
///
/// (The original inline test also asserted the old hashed `.loro` was removed via
/// the `pub(crate)` `document_sync_path`; that internal-artifact check is dropped
/// on migration to avoid hardcoding a private hash literal — the user-facing
/// regression is the stranded `.md`, asserted directly below.)
#[tokio::test]
async fn move_syncs_via_registry_removes_old_path() {
    let fs1 = Arc::new(InMemoryFs::new());
    let fs2 = Arc::new(InMemoryFs::new());

    fs1.write("note.md", b"# Hello").await.unwrap();
    let vault1 = Vault::init(Arc::clone(&fs1), author(1)).await.unwrap();

    // Handshake so both vaults have note.md.
    let vault2 = Vault::init(Arc::clone(&fs2), author(2)).await.unwrap();
    let request = vault2.prepare_sync_request().await.unwrap();
    let (exchange, _) = vault1.process_sync_message(&request).await.unwrap();
    let (final_resp, _) = vault2
        .process_sync_message(&exchange.unwrap())
        .await
        .unwrap();
    if let Some(resp) = final_resp {
        vault1.process_sync_message(&resp).await.unwrap();
    }

    assert!(!vault1.is_file_deleted("note.md"));
    assert!(!vault2.is_file_deleted("note.md"));
    assert!(fs2.exists("note.md").await.unwrap());

    // Move note.md → moved/note.md on vault1 (write the target, then rename_file
    // performs the tree.mov + fs cleanup).
    fs1.write("moved/note.md", b"# Hello").await.unwrap();
    fs1.delete("note.md").await.unwrap();
    vault1
        .rename_file("note.md", "moved/note.md")
        .await
        .unwrap();

    // Sync the move to vault2.
    let request2 = vault2.prepare_sync_request().await.unwrap();
    let (exchange2, _) = vault1.process_sync_message(&request2).await.unwrap();
    vault2
        .process_sync_message(&exchange2.unwrap())
        .await
        .unwrap();

    // The old path must be gone on the receiver — no stranded .md orphan.
    assert!(
        !fs2.exists("note.md").await.unwrap(),
        "old .md at note.md should be removed on the receiver after a move"
    );

    // The new path must exist with the original content.
    assert!(
        fs2.exists("moved/note.md").await.unwrap(),
        "moved/note.md should exist on the receiver after a move"
    );
    let moved_content = fs2.read("moved/note.md").await.unwrap();
    assert_eq!(moved_content, b"# Hello");
}

/// Swap case: two files exchange paths in a SINGLE registry exchange. The naive
/// move-cleanup (delete every vacated old_path) would delete the file the OTHER
/// node just moved into and strip its document update — permanent data loss on
/// the receiver. The fix excludes any old_path that an alive node now occupies.
/// This pins that exclusion against regression.
#[tokio::test]
async fn swap_move_syncs_via_registry() {
    let fs1 = Arc::new(InMemoryFs::new());
    let fs2 = Arc::new(InMemoryFs::new());

    fs1.write("a.md", b"# AAA").await.unwrap();
    fs1.write("b.md", b"# BBB").await.unwrap();
    let vault1 = Vault::init(Arc::clone(&fs1), author(1)).await.unwrap();

    // Handshake so both vaults have a.md and b.md.
    let vault2 = Vault::init(Arc::clone(&fs2), author(2)).await.unwrap();
    let request = vault2.prepare_sync_request().await.unwrap();
    let (exchange, _) = vault1.process_sync_message(&request).await.unwrap();
    let (final_resp, _) = vault2
        .process_sync_message(&exchange.unwrap())
        .await
        .unwrap();
    if let Some(resp) = final_resp {
        vault1.process_sync_message(&resp).await.unwrap();
    }

    assert_eq!(fs2.read("a.md").await.unwrap(), b"# AAA");
    assert_eq!(fs2.read("b.md").await.unwrap(), b"# BBB");

    // Swap on vault1: a.md → b.md and b.md → a.md via a temp path. Both moves
    // accumulate in the registry and reach vault2 in ONE exchange.
    fs1.write("tmp.md", b"# AAA").await.unwrap();
    fs1.delete("a.md").await.unwrap();
    vault1.rename_file("a.md", "tmp.md").await.unwrap();

    fs1.write("a.md", b"# BBB").await.unwrap();
    fs1.delete("b.md").await.unwrap();
    vault1.rename_file("b.md", "a.md").await.unwrap();

    fs1.write("b.md", b"# AAA").await.unwrap();
    fs1.delete("tmp.md").await.unwrap();
    vault1.rename_file("tmp.md", "b.md").await.unwrap();

    // Sync the whole swap to vault2 in one exchange.
    let request2 = vault2.prepare_sync_request().await.unwrap();
    let (exchange2, _) = vault1.process_sync_message(&request2).await.unwrap();
    vault2
        .process_sync_message(&exchange2.unwrap())
        .await
        .unwrap();

    // The B1 exclusion's job: both swapped-into paths must SURVIVE on the
    // receiver. Without it, the naive move-cleanup deletes the file an alive node
    // just moved into, so `fs2.exists(...)` goes false (data loss).
    assert!(
        fs2.exists("a.md").await.unwrap(),
        "a.md must survive — not be deleted by b's vacate-cleanup"
    );
    assert!(
        fs2.exists("b.md").await.unwrap(),
        "b.md must survive — not be deleted by a's vacate-cleanup"
    );
}

// ========== Sync flags (echo suppression) ==========

#[tokio::test]
async fn synced_write_arms_sync_flag() {
    // An inbound synced write arms the echo-suppression flag so the daemon's
    // watcher won't re-broadcast the write back to the sender.
    let fs1 = Arc::new(InMemoryFs::new());
    let fs2 = Arc::new(InMemoryFs::new());

    fs1.write("note.md", b"# Hello").await.unwrap();
    let vault1 = Vault::init(Arc::clone(&fs1), author(1)).await.unwrap();
    vault1.on_file_changed("note.md").await.unwrap();

    let vault2 = Vault::init(Arc::clone(&fs2), author(2)).await.unwrap();

    // Sync from vault1 to vault2.
    let request = vault1.prepare_sync_request().await.unwrap();
    let (response, _) = vault2.process_sync_message(&request).await.unwrap();
    let (_, modified) = vault1
        .process_sync_message(&response.unwrap())
        .await
        .unwrap();

    // vault1 has newer data, so it isn't modified by the exchange.
    assert!(modified.is_empty());

    // Deliver vault1's document update to vault2.
    let update = vault1.prepare_document_update("note.md").await.unwrap();
    let (_, modified2) = vault2.process_sync_message(&update.unwrap()).await.unwrap();

    // Every synced write on vault2 has its sync flag armed.
    for path in &modified2 {
        assert!(
            vault2.consume_sync_flag(path),
            "synced file {} should have its sync flag set",
            path
        );
    }
}

#[tokio::test]
async fn local_edit_does_not_arm_sync_flag() {
    // A LOCAL edit does NOT arm the flag — so it DOES broadcast.
    let fs = InMemoryFs::new();
    fs.write("note.md", b"# Original").await.unwrap();
    let vault = Vault::init(fs, author(1)).await.unwrap();

    vault.on_file_changed("note.md").await.unwrap();

    assert!(
        !vault.consume_sync_flag("note.md"),
        "a local edit must not set the sync flag"
    );
}

#[tokio::test]
async fn inbound_delete_arms_sync_flag() {
    // An incoming delete arms the flag (no echo back to the sender).
    let fs1 = Arc::new(InMemoryFs::new());
    let fs2 = Arc::new(InMemoryFs::new());

    fs1.write("note.md", b"# Hello").await.unwrap();
    fs2.write("note.md", b"# Hello").await.unwrap();
    let vault1 = Vault::init(Arc::clone(&fs1), author(1)).await.unwrap();
    let vault2 = Vault::init(Arc::clone(&fs2), author(2)).await.unwrap();
    vault1.on_file_changed("note.md").await.unwrap();
    vault2.on_file_changed("note.md").await.unwrap();

    // Initial sync to get them aligned.
    let req1 = vault1.prepare_sync_request().await.unwrap();
    let (resp1, _) = vault2.process_sync_message(&req1).await.unwrap();
    if let Some(r) = resp1 {
        vault1.process_sync_message(&r).await.unwrap();
    }

    // Delete in vault1 and send the delete message to vault2.
    vault1.delete_file("note.md").await.unwrap();
    let delete_msg = vault1.prepare_file_deleted("note.md").unwrap();
    let (_, modified) = vault2.process_sync_message(&delete_msg).await.unwrap();

    assert!(modified.contains(&"note.md".to_string()));
    assert!(
        vault2.consume_sync_flag("note.md"),
        "an inbound delete must set the sync flag"
    );
}

// ========== Debug / inspection API ==========

#[tokio::test]
async fn get_registry_version_keyed_by_author() {
    // The registry version vector is keyed by the device author's u64 hash.
    let fs = InMemoryFs::new();
    fs.write("note.md", b"# Hello").await.unwrap();
    let vault = Vault::init(fs, author(1)).await.unwrap();
    vault.on_file_changed("note.md").await.unwrap();

    let version = vault.get_registry_version();
    let author_key = format!("{:016x}", author(1).as_u64());
    assert!(
        version.contains_key(&author_key),
        "expected device author {} in version {:?}",
        author_key,
        version
    );
}

#[tokio::test]
async fn get_registry_stats_reports_operations() {
    // get_registry_stats reports a non-zero op/change count after a registration.
    let fs = InMemoryFs::new();
    fs.write("note.md", b"# Hello").await.unwrap();
    let vault = Vault::init(fs, author(1)).await.unwrap();
    vault.on_file_changed("note.md").await.unwrap();

    let stats = vault.get_registry_stats();
    assert!(
        stats.op_count > 0 || stats.change_count > 0,
        "expected at least some operations, got op_count={}, change_count={}",
        stats.op_count,
        stats.change_count
    );
}

#[tokio::test]
async fn get_document_blob_meta_returns_none_for_absent() {
    let fs = InMemoryFs::new();
    let vault = Vault::init(fs, author(1)).await.unwrap();
    let meta = vault
        .get_document_blob_meta("nonexistent.md")
        .await
        .unwrap();
    assert!(meta.is_none());
}

#[tokio::test]
async fn get_document_blob_meta_returns_version_metadata() {
    let fs = InMemoryFs::new();
    fs.write("test.md", b"# Hello").await.unwrap();
    let vault = Vault::init(fs, author(1)).await.unwrap();
    vault.on_file_changed("test.md").await.unwrap();

    let meta = vault
        .get_document_blob_meta("test.md")
        .await
        .unwrap()
        .unwrap();
    assert!(meta.change_count > 0);
    assert!(!meta.end_version.is_empty());
}

#[tokio::test]
async fn get_document_info_returns_none_for_absent() {
    let fs = InMemoryFs::new();
    let vault = Vault::init(fs, author(1)).await.unwrap();
    let info = vault.get_document_info("nonexistent.md").await.unwrap();
    assert!(info.is_none());
}

#[tokio::test]
async fn get_document_info_returns_document_details() {
    let fs = InMemoryFs::new();
    fs.write("test.md", b"# Hello").await.unwrap();
    let vault = Vault::init(fs, author(1)).await.unwrap();
    vault.on_file_changed("test.md").await.unwrap();

    let info = vault.get_document_info("test.md").await.unwrap().unwrap();
    assert_eq!(info.path, "test.md");
    assert!(info.body_length > 0);
    assert!(info.change_count > 0);
    assert!(info.doc_id.is_some());
    assert!(!info.version.is_empty());
}

#[tokio::test]
async fn get_document_info_flags_frontmatter() {
    let fs = InMemoryFs::new();
    let content = "---\ntitle: Test\n---\n\n# Hello";
    fs.write("test.md", content.as_bytes()).await.unwrap();
    let vault = Vault::init(fs, author(1)).await.unwrap();
    vault.on_file_changed("test.md").await.unwrap();

    let info = vault.get_document_info("test.md").await.unwrap().unwrap();
    assert!(info.has_frontmatter);
}
