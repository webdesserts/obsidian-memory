//! Integration tests for the registry-debris inspector (`find_registry_debris`).
//!
//! These cover the publicly-drivable debris case: a healthy node (one alive node
//! with its `.md` on disk) yields an empty report. Driven through the public
//! `Vault` API plus a retained `Arc<InMemoryFs>` handle.
//!
//! The duplicate-alive-pair and `apply_dedupe` tests stay inline in
//! `vault/mod.rs`: forging two alive nodes at one path requires
//! `registry()`/`registry_mut().import()`/`rebuild_path_cache()` (`pub(crate)`),
//! and the apply/relic assertions read `file_tree()` or strip the hashed `.loro`
//! via `document_sync_path()` (`pub(crate)`) — none expressible through the
//! public API.

mod common;

use std::sync::Arc;

use common::author;
use sync_core::Vault;
use sync_core::fs::{FileSystem, InMemoryFs};

#[tokio::test]
async fn find_registry_debris_ignores_healthy_node() {
    // A normal path — one alive node with its .md on disk — is neither a
    // duplicate nor a relic. An operator running a dry run on a clean vault sees
    // an empty report.
    let fs = Arc::new(InMemoryFs::new());

    fs.write("healthy.md", b"# Healthy").await.unwrap();
    let vault = Vault::init(Arc::clone(&fs), author(1)).await.unwrap();
    vault.register_file("healthy.md").unwrap();
    vault.save_registry().await.unwrap();

    let report = vault.find_registry_debris().await.unwrap();

    assert!(
        report.duplicate_groups.is_empty(),
        "a single-alive-node path must not be a duplicate group, got: {:?}",
        report.duplicate_groups
    );
    assert!(
        report.relics.is_empty(),
        "a node with its .md on disk must not be a relic, got: {:?}",
        report.relics
    );
}
