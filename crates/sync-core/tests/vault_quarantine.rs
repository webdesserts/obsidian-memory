//! Integration tests for orphan quarantine during reconcile.
//!
//! Reconcile moves untracked disk orphans — `.md` files on disk whose registry
//! state is tombstoned — to `.trash/<path>`. These cover the happy path, the
//! safety guards (a registry-absent file is indexed, never quarantined; an
//! alive-node relic with no disk file is left untouched), idempotency, and the
//! crash-recovery paths: a per-orphan quarantine write failure must not abort
//! `Vault::load`, and a partial-failure orphan at both paths must reuse the
//! existing trash copy rather than allocate unbounded `.N` suffixes. All drive
//! the public `Vault` API plus a retained `Arc<InMemoryFs>` handle; two tests
//! use a `TrashWriteFailingFs` fake to inject the write failure.
//!
//! Tests that assert on the `pub(crate)` registry-tombstone state directly (the
//! meta-less-tombstone and duplicate-node-pair guards, plus the direct
//! `quarantine_orphan` call) stay inline in `vault/mod.rs` — those preconditions
//! can't be expressed through the public API.

mod common;

use std::sync::Arc;

use common::author;
use sync_core::fs::{FileEntry, FileStat, FileSystem, FsError, InMemoryFs};
use sync_core::Vault;

/// Seed a tombstoned-with-meta orphan and return a loaded vault whose
/// `deleted_paths` is repopulated from the persisted tombstone, with the orphan
/// markdown NOT yet on disk. The caller writes the orphan strand back to disk and
/// drives `reconcile()` itself so it can assert on the returned report.
///
/// (The orphan must be off disk at load time, otherwise `Vault::load`'s own
/// reconcile quarantines it before the caller's explicit reconcile runs — the
/// load-time path is covered by the NativeFs integration test.)
async fn seed_tombstoned_orphan(
    fs: &Arc<InMemoryFs>,
    path: &str,
    content: &[u8],
) -> Vault<Arc<InMemoryFs>> {
    fs.write(path, content).await.unwrap();
    let vault = Vault::init(Arc::clone(fs), author(1)).await.unwrap();
    fs.delete(path).await.unwrap();
    vault.delete_file(path).await.unwrap();
    drop(vault);

    // Load fresh (orphan still off disk) so rebuild_path_cache repopulates
    // deleted_paths from the persisted tombstone and load's reconcile is a no-op.
    Vault::load(Arc::clone(fs), author(1)).await.unwrap()
}

#[tokio::test]
async fn reconcile_quarantines_tombstoned_orphan() {
    // A tombstoned disk orphan moves to .trash/, is NOT re-indexed, and the path
    // stays absent from the alive set.
    let fs = Arc::new(InMemoryFs::new());

    let vault = seed_tombstoned_orphan(&fs, "orphan.md", b"# Orphan").await;
    // Write the orphan strand back to disk, then reconcile.
    fs.write("orphan.md", b"# Orphan").await.unwrap();
    let report = vault.reconcile().await.unwrap();

    assert!(
        report.quarantined.contains(&"orphan.md".to_string()),
        "tombstoned orphan should be quarantined, got: {:?}",
        report.quarantined
    );
    assert!(
        !report.indexed.contains(&"orphan.md".to_string()),
        "a tombstoned orphan must never be indexed as a new file"
    );

    // Original gone from its path, present under .trash/ on disk.
    assert!(
        !fs.exists("orphan.md").await.unwrap(),
        "the orphan must be removed from its original path"
    );
    assert!(
        fs.exists(".trash/orphan.md").await.unwrap(),
        "the orphan must be moved under .trash/"
    );

    // Quarantine is disk-only — it must not register an alive node for the path.
    assert!(
        vault.is_file_deleted("orphan.md"),
        "quarantine must leave the path tombstoned, not register an alive node"
    );
}

#[tokio::test]
async fn reconcile_does_not_quarantine_registry_absent_file() {
    // A disk file with no registry entry at all (never deleted) is indexed as
    // new, never quarantined. Regression guard against over-deletion.
    let fs = Arc::new(InMemoryFs::new());

    let _vault = Vault::init(Arc::clone(&fs), author(1)).await.unwrap();
    fs.write("brand_new.md", b"# Brand New").await.unwrap();

    let vault = Vault::load(Arc::clone(&fs), author(1)).await.unwrap();
    let report = vault.reconcile().await.unwrap();

    assert!(
        !report.quarantined.contains(&"brand_new.md".to_string()),
        "a registry-absent file must not be quarantined"
    );
    assert!(
        fs.exists("brand_new.md").await.unwrap(),
        "the absent file must stay on disk and be indexed"
    );
    assert!(
        !fs.exists(".trash/brand_new.md").await.unwrap(),
        "no .trash entry should be created for an absent file"
    );
}

#[tokio::test]
async fn reconcile_alive_node_no_disk_file_is_noop() {
    // An alive registry node whose path has no disk file is a registry-debris
    // relic, not a disk orphan. Reconcile leaves it untouched and must not crash.
    let fs = Arc::new(InMemoryFs::new());

    fs.write("relic.md", b"# Relic").await.unwrap();
    let vault = Vault::init(Arc::clone(&fs), author(1)).await.unwrap();
    // Remove the markdown but leave the alive node in the registry (no delete_file).
    fs.delete("relic.md").await.unwrap();

    let report = vault.reconcile().await.unwrap();
    assert!(
        report.quarantined.is_empty(),
        "an alive-node-no-disk-file relic must not be quarantined, got: {:?}",
        report.quarantined
    );
    assert!(
        !fs.exists(".trash/relic.md").await.unwrap(),
        "no .trash entry for a relic with no disk file"
    );
}

#[tokio::test]
async fn reconcile_quarantine_failure_does_not_abort_load() {
    // B1 regression: reconcile runs inside Vault::load, so a per-orphan quarantine
    // failure must NOT propagate and abort daemon startup. The failing orphan is
    // logged and skipped; other files still reconcile and the vault loads.

    // Seed a tombstoned orphan and a separate brand-new file in InMemoryFs.
    let seed = Arc::new(InMemoryFs::new());
    seed.write("orphan.md", b"# Orphan").await.unwrap();
    let vault = Vault::init(Arc::clone(&seed), author(1)).await.unwrap();
    seed.delete("orphan.md").await.unwrap();
    vault.delete_file("orphan.md").await.unwrap();
    drop(vault);
    // Recreate the orphan strand and add a genuinely new file.
    seed.write("orphan.md", b"# Orphan").await.unwrap();
    seed.write("fresh.md", b"# Fresh").await.unwrap();

    // Wrap the SAME underlying fs so quarantine's write into .trash/ fails.
    // Load must still succeed.
    let failing = Arc::new(TrashWriteFailingFs {
        inner: Arc::clone(&seed),
    });
    let vault = Vault::load(Arc::clone(&failing), author(1))
        .await
        .expect("Vault::load must succeed even when a quarantine write fails");

    // The new file was still indexed despite the orphan's quarantine failing.
    let files = vault.list_files().await.unwrap();
    assert!(
        files.contains(&"fresh.md".to_string()),
        "a brand-new file must still be indexed when a sibling orphan fails to \
         quarantine; got: {:?}",
        files
    );

    // The orphan was NOT quarantined (its .trash write failed) and was NOT
    // resurrected (the gate took the quarantine branch, not the index branch),
    // so it stays on disk for the next reconcile to retry.
    let report = vault.reconcile().await.unwrap();
    assert!(
        !report.quarantined.contains(&"orphan.md".to_string()),
        "the failing orphan must not be reported as quarantined"
    );
    assert!(
        !report.indexed.contains(&"orphan.md".to_string()),
        "the failing orphan must not be resurrected as a new index entry"
    );
}

#[tokio::test]
async fn reconcile_quarantine_recovers_from_partial_failure_idempotently() {
    // Crash-idempotency: if a prior quarantine wrote .trash/<path> but failed to
    // delete the original (the non-atomic write→delete window), the orphan sits at
    // BOTH paths. The next reconcile must reuse the existing identical trash copy
    // and retry the delete — NOT allocate a new collision suffix. Without this,
    // .trash/<path>.N would grow without bound under a persistent delete failure.
    let fs = Arc::new(InMemoryFs::new());

    // Tombstone orphan.md, then load a fresh vault so deleted_paths is repopulated
    // (orphan off disk at load → load's reconcile is a no-op).
    fs.write("orphan.md", b"# Orphan").await.unwrap();
    let vault = Vault::init(Arc::clone(&fs), author(1)).await.unwrap();
    fs.delete("orphan.md").await.unwrap();
    vault.delete_file("orphan.md").await.unwrap();
    drop(vault);
    let vault = Vault::load(Arc::clone(&fs), author(1)).await.unwrap();

    // Construct the post-crash partial-failure state directly: identical content at
    // both the original path AND the trash destination (write succeeded, delete did
    // not).
    fs.write("orphan.md", b"# Orphan").await.unwrap();
    fs.write(".trash/orphan.md", b"# Orphan").await.unwrap();

    let report = vault.reconcile().await.unwrap();

    // The retry deleted the original and reported the quarantine complete.
    assert!(
        report.quarantined.contains(&"orphan.md".to_string()),
        "the partial-failure orphan should report as quarantined once the delete \
         retry succeeds, got: {:?}",
        report.quarantined
    );
    assert!(
        !fs.exists("orphan.md").await.unwrap(),
        "the original must be removed on the retry"
    );
    // The existing trash copy was reused — NO new collision-suffixed duplicate.
    assert!(
        fs.exists(".trash/orphan.md").await.unwrap(),
        ".trash/orphan.md must remain"
    );
    assert!(
        !fs.exists(".trash/orphan.md.1").await.unwrap(),
        "a new collision suffix must NOT be created when the trash copy is identical"
    );
}

#[tokio::test]
async fn reconcile_quarantine_is_idempotent() {
    // A second reconcile pass quarantines nothing: the first moved the orphan
    // under .trash/, and list_files excludes dot-directories, so the moved file is
    // no longer a candidate. Verify exactly one trash entry, no nesting.
    let fs = Arc::new(InMemoryFs::new());

    let vault = seed_tombstoned_orphan(&fs, "dupe.md", b"# Dupe").await;
    fs.write("dupe.md", b"# Dupe").await.unwrap();

    let first = vault.reconcile().await.unwrap();
    assert!(first.quarantined.contains(&"dupe.md".to_string()));

    let second = vault.reconcile().await.unwrap();
    assert!(
        second.quarantined.is_empty(),
        "second pass must quarantine nothing, got: {:?}",
        second.quarantined
    );
    assert!(
        fs.exists(".trash/dupe.md").await.unwrap(),
        ".trash/dupe.md must still exist after the second pass"
    );
    assert!(
        !fs.exists(".trash/.trash/dupe.md").await.unwrap(),
        "trash contents must never be re-quarantined into a nested .trash/"
    );
}

/// Test fs that delegates to a shared InMemoryFs but fails writes into
/// `.trash/`, simulating a real-fs quarantine failure (full disk, permission
/// error). Wraps the same Arc the seed state was written through.
struct TrashWriteFailingFs {
    inner: Arc<InMemoryFs>,
}

#[cfg_attr(target_arch = "wasm32", async_trait::async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait::async_trait)]
impl FileSystem for TrashWriteFailingFs {
    async fn read(&self, path: &str) -> sync_core::fs::Result<Vec<u8>> {
        self.inner.read(path).await
    }
    async fn write(&self, path: &str, content: &[u8]) -> sync_core::fs::Result<()> {
        if path.starts_with(".trash/") {
            return Err(FsError::Io("simulated trash write failure".into()));
        }
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
