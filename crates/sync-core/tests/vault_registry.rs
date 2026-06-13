//! Integration tests for vault registry persistence + path validation.
//!
//! Persistence: a local registry mutation (register / delete / rename) must
//! survive a process restart — the in-memory registry LoroDoc is written to disk
//! after each mutation so a fresh `Vault::load` recovers the same state. A
//! corrupt persisted registry must hard-fail the load rather than silently
//! re-indexing (which would diverge every file from its peers). Path validation:
//! the mutators reject traversal / null-byte / non-markdown / empty / too-long
//! paths through their public error contract.
//!
//! These drive the public `Vault` API. Where the original inline tests asserted
//! `path_to_node().contains_key(p)` (a `pub(crate)` reach-in), this uses the
//! public `!is_file_deleted(p)` instead — it returns `true` for a path with no
//! alive node, so `!is_file_deleted` is the public proxy for "present in the
//! alive tree". The `.sync/...` and registry-tree literals are hardcoded because
//! the consts that name them are `pub(crate)`.

mod common;

use std::sync::Arc;

use common::author;
use sync_core::fs::{FileSystem, InMemoryFs};
use sync_core::vault::VaultError;
use sync_core::Vault;

const REGISTRY_FILE: &str = ".sync/registry.loro";
const REGISTRY_TREE: &str = "files";

// ========== Registry persistence ==========

#[tokio::test]
async fn register_file_survives_reload() {
    // The bare register + caller-flush contract: register_file mutates the tree
    // in memory only, and the caller's save_registry() is what makes the node
    // durable. Isolate the contract from init's batched index save AND from
    // reconcile re-registering on reload:
    //   - init with NO pre-existing file, so the index pass registers nothing.
    //   - register the file AFTER init, then flush explicitly via save_registry().
    //   - delete the markdown before reloading, so reconcile has nothing to
    //     re-register — a node present after reload can only come from the
    //     persisted registry snapshot, not a fresh index pass.
    let fs = Arc::new(InMemoryFs::new());
    let vault = Vault::init(Arc::clone(&fs), author(1)).await.unwrap();

    fs.write("note.md", b"# Hello").await.unwrap();
    vault.register_file("note.md").unwrap();
    vault.save_registry().await.unwrap();

    // Remove the markdown so reload's reconcile can't re-register it from disk.
    fs.delete("note.md").await.unwrap();
    drop(vault);

    let vault2 = Vault::load(Arc::clone(&fs), author(1)).await.unwrap();
    assert!(
        !vault2.is_file_deleted("note.md"),
        "registered path must survive a reload via the persisted registry"
    );
}

#[tokio::test]
async fn delete_file_tombstone_survives_reload() {
    // A deletion tombstone must persist so peers importing the saved registry see
    // the deletion op. After reload the path is absent from the alive set, and a
    // fresh peer importing the saved registry must also see the node as deleted
    // (the tombstone is carried in the CRDT snapshot).
    //
    // Mirrors the daemon sequence: the OS/user deletes the markdown first, then
    // delete_file records the CRDT tombstone, so reconcile during reload has
    // nothing to re-register.
    let fs = Arc::new(InMemoryFs::new());

    fs.write("note.md", b"# Hello").await.unwrap();
    let vault = Vault::init(Arc::clone(&fs), author(1)).await.unwrap();

    fs.delete("note.md").await.unwrap();
    vault.delete_file("note.md").await.unwrap();

    // Grab the registry snapshot to verify it carries the tombstone op.
    let saved_bytes = fs.read(REGISTRY_FILE).await.unwrap();
    drop(vault);

    // Reload — the path must still be gone from the alive set.
    let vault2 = Vault::load(Arc::clone(&fs), author(1)).await.unwrap();
    assert!(
        vault2.is_file_deleted("note.md"),
        "deleted path must not reappear in the alive set after reload"
    );

    // A fresh peer importing the saved registry must also see the node deleted.
    let peer_doc = loro::LoroDoc::new();
    peer_doc.import(&saved_bytes).unwrap();
    let peer_tree = peer_doc.get_tree(REGISTRY_TREE);
    let any_alive = peer_tree
        .nodes()
        .into_iter()
        .filter(|id| !peer_tree.is_node_deleted(id).unwrap_or(true))
        .count();
    assert_eq!(
        any_alive, 0,
        "saved registry must carry the deletion tombstone for peers"
    );
}

#[tokio::test]
async fn rename_file_survives_reload() {
    // A rename must persist the updated registry so the new path (not the old)
    // survives a reload. Simulates the real watcher sequence: the OS moves the
    // file (old gone, new exists), then rename_file records the CRDT op.
    let fs = Arc::new(InMemoryFs::new());

    fs.write("old.md", b"# Content").await.unwrap();
    let vault = Vault::init(Arc::clone(&fs), author(1)).await.unwrap();

    // Simulate the OS-level rename: delete old, create new on the filesystem.
    fs.delete("old.md").await.unwrap();
    fs.write("new.md", b"# Content").await.unwrap();
    vault.rename_file("old.md", "new.md").await.unwrap();
    drop(vault);

    let vault2 = Vault::load(Arc::clone(&fs), author(1)).await.unwrap();
    assert!(
        !vault2.is_file_deleted("new.md"),
        "renamed path must survive reload in the alive set"
    );
    assert!(
        vault2.is_file_deleted("old.md"),
        "old path must not be in the alive set after reload"
    );
}

#[tokio::test]
async fn reconcile_batch_registrations_survive_reload() {
    // Reconcile can register hundreds of files during startup. The registrations
    // must be batched (one save at the end of reconcile, not per-file) and must
    // survive a second load.
    let fs = Arc::new(InMemoryFs::new());

    fs.write("a.md", b"# A").await.unwrap();
    fs.write("b.md", b"# B").await.unwrap();
    fs.write("c.md", b"# C").await.unwrap();

    // init calls index_existing_files, which registers via on_file_changed.
    let _vault = Vault::init(Arc::clone(&fs), author(1)).await.unwrap();
    drop(_vault);

    let vault2 = Vault::load(Arc::clone(&fs), author(1)).await.unwrap();
    assert!(!vault2.is_file_deleted("a.md"), "a.md must survive reload");
    assert!(!vault2.is_file_deleted("b.md"), "b.md must survive reload");
    assert!(!vault2.is_file_deleted("c.md"), "c.md must survive reload");
}

#[tokio::test]
async fn load_with_corrupt_registry_returns_error() {
    // A corrupt registry.loro must HARD-FAIL the load rather than silently
    // falling back to an empty registry. An empty registry re-indexes every file
    // with fresh doc_ids → mass divergence from peers → latest-wins content
    // clobber. Failing loud is the only safe behavior.
    let fs = Arc::new(InMemoryFs::new());

    fs.write("note.md", b"# Note").await.unwrap();
    let _vault = Vault::init(Arc::clone(&fs), author(1)).await.unwrap();

    // Corrupt the persisted registry.
    fs.write(REGISTRY_FILE, b"not a valid loro snapshot")
        .await
        .unwrap();

    let result = Vault::load(Arc::clone(&fs), author(1)).await;
    assert!(
        matches!(result, Err(VaultError::CorruptRegistry(_))),
        "corrupt registry must fail with CorruptRegistry, got: {:?}",
        result.map(|_| "Ok(vault)")
    );
}

// ========== Path validation ==========

#[tokio::test]
async fn delete_file_succeeds_silently_on_untracked_path() {
    // delete_file on a path with no registry node returns Ok (silent success),
    // NOT an error — the daemon's watcher fires deletes for paths the registry
    // may not track, and those must not surface as failures.
    let fs = InMemoryFs::new();
    let vault = Vault::init(fs, author(1)).await.unwrap();

    let result = vault.delete_file("untracked.md").await;
    assert!(
        result.is_ok(),
        "delete of untracked path must succeed silently"
    );
}

#[tokio::test]
async fn delete_file_removes_from_tree() {
    // Delete marks the file deleted.
    let fs = InMemoryFs::new();
    fs.write("note.md", b"# Hello").await.unwrap();
    let vault = Vault::init(fs, author(1)).await.unwrap();
    vault.on_file_changed("note.md").await.unwrap();

    assert!(!vault.is_file_deleted("note.md"));

    vault.delete_file("note.md").await.unwrap();

    assert!(vault.is_file_deleted("note.md"));
}

#[tokio::test]
async fn rename_file_updates_tree() {
    // Rename makes the new path live.
    let fs = Arc::new(InMemoryFs::new());
    fs.write("old.md", b"# Content").await.unwrap();
    let vault = Vault::init(Arc::clone(&fs), author(1)).await.unwrap();
    vault.on_file_changed("old.md").await.unwrap();

    assert!(!vault.is_file_deleted("old.md"));

    // Create the target file and rename.
    fs.write("new.md", b"# Content").await.unwrap();
    vault.rename_file("old.md", "new.md").await.unwrap();

    // The new path is now live (the node was moved, not deleted).
    assert!(!vault.is_file_deleted("new.md"));
}

#[tokio::test]
async fn path_traversal_rejected() {
    // `../` paths are refused by every mutator — the security boundary tested
    // through the public API rather than the internal validate_sync_path.
    let fs = InMemoryFs::new();
    let vault = Vault::init(fs, author(1)).await.unwrap();

    assert!(vault.delete_file("../secret.md").await.is_err());
    assert!(vault.rename_file("note.md", "../secret.md").await.is_err());
    assert!(vault.register_file("../evil.md").is_err());
}

#[tokio::test]
async fn null_byte_rejected() {
    // Null-byte paths are refused.
    let fs = InMemoryFs::new();
    let vault = Vault::init(fs, author(1)).await.unwrap();

    assert!(vault.delete_file("foo\0.md").await.is_err());
    assert!(vault.register_file("bar\0.md").is_err());
}

#[tokio::test]
async fn non_markdown_rejected() {
    // Non-`.md` paths are refused.
    let fs = InMemoryFs::new();
    let vault = Vault::init(fs, author(1)).await.unwrap();

    assert!(vault.register_file("script.js").is_err());
    assert!(vault.delete_file("image.png").await.is_err());
}

#[tokio::test]
async fn empty_path_rejected() {
    // Empty paths are refused.
    let fs = InMemoryFs::new();
    let vault = Vault::init(fs, author(1)).await.unwrap();

    assert!(vault.register_file("").is_err());
    assert!(vault.delete_file("").await.is_err());
}

#[tokio::test]
async fn empty_segment_rejected() {
    // `a//b.md` (empty path segment) is refused.
    let fs = InMemoryFs::new();
    let vault = Vault::init(fs, author(1)).await.unwrap();

    assert!(vault.register_file("a//b.md").is_err());
    assert!(vault.delete_file("foo//bar.md").await.is_err());
}

#[tokio::test]
async fn path_too_long_rejected() {
    // A path over 1024 chars is refused.
    let fs = InMemoryFs::new();
    let vault = Vault::init(fs, author(1)).await.unwrap();

    let long_path = format!("{}.md", "a".repeat(1025));
    assert!(vault.register_file(&long_path).is_err());
}
