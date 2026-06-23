//! Integration tests for vault reconcile (offline-edit detection on load).
//!
//! These exercise what a user sees when the plugin was off and files changed on
//! disk underneath it: new files get indexed, modified files re-index, deleted
//! files drop from `list_files`, moved files are found at the new path only, and
//! orphan reports name the real path. A file that vanishes mid-scan must not
//! abort `Vault::load`. All drive the public `Vault` API plus a retained
//! `Arc<InMemoryFs>` handle for the offline edits; one test wraps the fs in a
//! `VanishOnReadFs` fake to reproduce the startup-scan race.

mod common;

use std::sync::Arc;

use common::author;
use sync_core::Vault;
use sync_core::fs::{FileEntry, FileStat, FileSystem, FsError, InMemoryFs};

#[tokio::test]
async fn vault_init_reports_initialized() {
    // A fresh vault reports initialized.
    let fs = InMemoryFs::new();
    let vault = Vault::init(fs, author(1)).await.unwrap();

    assert!(vault.is_initialized().await.unwrap());
}

#[tokio::test]
async fn file_change_becomes_retrievable_document() {
    // An edited file becomes a retrievable document.
    let fs = InMemoryFs::new();
    fs.write("test.md", b"# Hello\n\nWorld").await.unwrap();

    let vault = Vault::init(fs, author(1)).await.unwrap();
    vault.on_file_changed("test.md").await.unwrap();

    let doc = vault.get_document("test.md").await.unwrap();
    assert!(doc.to_markdown().contains("Hello"));
}

#[tokio::test]
async fn reconcile_detects_new_files() {
    // A file added while the plugin was off is indexed on load.
    let fs = Arc::new(InMemoryFs::new());

    fs.write("existing.md", b"# Existing").await.unwrap();
    let _vault = Vault::init(Arc::clone(&fs), author(1)).await.unwrap();

    // A new file appears while the plugin was off.
    fs.write("new_file.md", b"# New File").await.unwrap();

    let vault = Vault::load(Arc::clone(&fs), author(1)).await.unwrap();
    let doc = vault.get_document("new_file.md").await.unwrap();
    assert!(doc.to_markdown().contains("New File"));
}

#[tokio::test]
async fn reconcile_detects_modified_files() {
    // A file modified while the plugin was off re-indexes on load.
    let fs = Arc::new(InMemoryFs::new());

    fs.write("note.md", b"# Original Content").await.unwrap();
    let _vault = Vault::init(Arc::clone(&fs), author(1)).await.unwrap();

    fs.write("note.md", b"# Modified Content").await.unwrap();

    let vault = Vault::load(Arc::clone(&fs), author(1)).await.unwrap();
    let doc = vault.get_document("note.md").await.unwrap();
    assert!(doc.to_markdown().contains("Modified Content"));
}

#[tokio::test]
async fn reconcile_detects_deleted_files() {
    // A file deleted while the plugin was off drops from list_files on load.
    let fs = Arc::new(InMemoryFs::new());

    fs.write("keep.md", b"# Keep this").await.unwrap();
    fs.write("delete.md", b"# Delete this").await.unwrap();
    let _vault = Vault::init(Arc::clone(&fs), author(1)).await.unwrap();

    fs.delete("delete.md").await.unwrap();

    let vault = Vault::load(Arc::clone(&fs), author(1)).await.unwrap();
    let files = vault.list_files().await.unwrap();
    assert!(!files.contains(&"delete.md".to_string()));
    assert!(files.contains(&"keep.md".to_string()));
}

#[tokio::test]
async fn reconcile_orphan_report_uses_real_path_not_empty() {
    // When a markdown file is deleted offline, its orphaned `.loro` must be
    // reported under the file's real path, not an empty string. (The orphan
    // loader previously passed "" to from_bytes, clobbering META_PATH to "".)
    let fs = Arc::new(InMemoryFs::new());

    fs.write("notes/deleted.md", b"# Deleted Note")
        .await
        .unwrap();
    let _vault = Vault::init(Arc::clone(&fs), author(1)).await.unwrap();

    // Delete the markdown offline; the .loro orphan remains on disk.
    fs.delete("notes/deleted.md").await.unwrap();

    let vault = Vault::load(Arc::clone(&fs), author(1)).await.unwrap();
    let report = vault.reconcile().await.unwrap();

    assert!(
        report.orphaned.contains(&"notes/deleted.md".to_string()),
        "orphan report should contain the real path, got: {:?}",
        report.orphaned
    );
    assert!(
        !report.orphaned.contains(&String::new()),
        "orphan report must not contain an empty path, got: {:?}",
        report.orphaned
    );
}

#[tokio::test]
async fn reconcile_detects_file_move() {
    // A file renamed on disk while the plugin was off is found at the new path
    // with content preserved; the old path is gone. (The `.loro` migration
    // itself is covered on the receiver side by the sync test
    // `move_syncs_via_registry_removes_old_path`, so this test asserts only the
    // user-visible effect, not the internal hashed `.loro` file names.)
    let fs = Arc::new(InMemoryFs::new());

    fs.write("old_name.md", b"# Unique Content ABC123")
        .await
        .unwrap();
    let _vault = Vault::init(Arc::clone(&fs), author(1)).await.unwrap();

    // Simulate an offline rename.
    let content = fs.read("old_name.md").await.unwrap();
    fs.write("new_name.md", &content).await.unwrap();
    fs.delete("old_name.md").await.unwrap();

    let vault = Vault::load(Arc::clone(&fs), author(1)).await.unwrap();

    // The new file is accessible with the same content.
    let doc = vault.get_document("new_name.md").await.unwrap();
    assert!(doc.to_markdown().contains("Unique Content ABC123"));

    // The old path is gone, the new one present.
    let files = vault.list_files().await.unwrap();
    assert!(!files.contains(&"old_name.md".to_string()));
    assert!(files.contains(&"new_name.md".to_string()));
}

#[tokio::test]
async fn reconcile_detects_file_move_to_subfolder() {
    // A file moved into a subfolder offline is found at the new path only.
    let fs = Arc::new(InMemoryFs::new());

    fs.write("note.md", b"# My Note XYZ789").await.unwrap();
    let _vault = Vault::init(Arc::clone(&fs), author(1)).await.unwrap();

    // Simulate an offline move into a subfolder.
    let content = fs.read("note.md").await.unwrap();
    fs.mkdir("knowledge").await.unwrap();
    fs.write("knowledge/note.md", &content).await.unwrap();
    fs.delete("note.md").await.unwrap();

    let vault = Vault::load(Arc::clone(&fs), author(1)).await.unwrap();

    let doc = vault.get_document("knowledge/note.md").await.unwrap();
    assert!(doc.to_markdown().contains("My Note XYZ789"));

    let files = vault.list_files().await.unwrap();
    assert!(!files.contains(&"note.md".to_string()));
    assert!(files.contains(&"knowledge/note.md".to_string()));
}

#[tokio::test]
async fn reconcile_skips_race_deleted_file() {
    // A markdown file deleted between list_files() and the per-file reconcile
    // body (a startup-scan race) must NOT abort Vault::load. The surviving files
    // reconcile normally; the vanished one is skipped.
    let fs = Arc::new(InMemoryFs::new());

    fs.write("keep.md", b"# Keep").await.unwrap();
    let _vault = Vault::init(Arc::clone(&fs), author(1)).await.unwrap();

    // A brand-new file appears (no .loro yet). The wrapping filesystem lets
    // list_files() still enumerate it but returns FsError::NotFound when reconcile
    // reads it to index — exactly the race where a file vanishes between the
    // directory scan and the per-file body.
    fs.write("racy.md", b"# Racy").await.unwrap();
    let race_fs = Arc::new(VanishOnReadFs::new(Arc::clone(&fs), "racy.md"));

    // Before the fix: NotFound propagates through reconcile → Vault::load → Err.
    // After: the file is skipped (debug-logged), load succeeds, survivor present.
    let vault = Vault::load(race_fs, author(1))
        .await
        .expect("Vault::load must survive a race-deleted file during reconcile");

    let files = vault.list_files().await.unwrap();
    assert!(
        files.contains(&"keep.md".to_string()),
        "surviving file should still reconcile, got: {:?}",
        files
    );
}

/// Test fs that delegates to a shared InMemoryFs but returns
/// `FsError::NotFound` when reading a specific armed path, simulating a file
/// that was deleted after `list_files()` enumerated it but before the per-file
/// reconcile body read it (the startup-scan race).
struct VanishOnReadFs {
    inner: Arc<InMemoryFs>,
    vanish_path: String,
}

impl VanishOnReadFs {
    fn new(inner: Arc<InMemoryFs>, vanish_path: &str) -> Self {
        Self {
            inner,
            vanish_path: vanish_path.to_string(),
        }
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl FileSystem for VanishOnReadFs {
    async fn read(&self, path: &str) -> sync_core::fs::Result<Vec<u8>> {
        if path == self.vanish_path {
            return Err(FsError::NotFound(path.to_string()));
        }
        self.inner.read(path).await
    }
    async fn write(&self, path: &str, content: &[u8]) -> sync_core::fs::Result<()> {
        self.inner.write(path, content).await
    }
    async fn list(&self, path: &str) -> sync_core::fs::Result<Vec<FileEntry>> {
        self.inner.list(path).await
    }
    async fn delete(&self, path: &str) -> sync_core::fs::Result<()> {
        self.inner.delete(path).await
    }
    async fn exists(&self, path: &str) -> sync_core::fs::Result<bool> {
        self.inner.exists(path).await
    }
    async fn stat(&self, path: &str) -> sync_core::fs::Result<FileStat> {
        self.inner.stat(path).await
    }
    async fn mkdir(&self, path: &str) -> sync_core::fs::Result<()> {
        self.inner.mkdir(path).await
    }
    async fn rename(&self, from: &str, to: &str) -> sync_core::fs::Result<()> {
        self.inner.rename(from, to).await
    }
}
