//! Integration tests for vault metadata + migration.
//!
//! These exercise the user-facing effects of `.sync/metadata.toml`: a fresh
//! vault writes a valid metadata file with a vault id, that id survives a
//! reload, vault-id adoption (pairing) persists without bumping the format
//! version, a legacy v0 vault (no metadata.toml) migrates to v1 on load, and a
//! future-version or corrupt metadata file is rejected with a hard error. All
//! drive the public `Vault` API plus a retained `Arc<InMemoryFs>` handle for
//! reading/seeding the on-disk metadata file.
//!
//! The `.sync/...` path literals are hardcoded here because the consts that name
//! them (`METADATA_FILE`, `SYNC_DIR`) are `pub(crate)` and unreachable from an
//! integration test. The migrated tests are the de-facto tripwire for those
//! paths — they'd fail if the layout drifted.

mod common;

use std::sync::Arc;

use common::author;
use sync_core::fs::{FileSystem, InMemoryFs};
use sync_core::{Vault, VaultId};

const METADATA_FILE: &str = ".sync/metadata.toml";
const SYNC_DIR: &str = ".sync";

#[tokio::test]
async fn init_writes_metadata_with_vault_id() {
    // A fresh vault writes a valid metadata.toml carrying version 1 and the
    // vault's own id.
    let fs = Arc::new(InMemoryFs::new());
    let vault = Vault::init(Arc::clone(&fs), author(1)).await.unwrap();

    assert!(fs.exists(METADATA_FILE).await.unwrap());

    let bytes = fs.read(METADATA_FILE).await.unwrap();
    let toml_str = String::from_utf8(bytes).unwrap();
    // The on-disk TOML uses the `version` / `vault_id` keys and reports version 1.
    assert!(toml_str.contains("version = 1"));
    assert!(toml_str.contains("vault_id = "));
    // And the persisted id matches the live vault.
    assert!(toml_str.contains(&vault.vault_id().to_string()));
}

#[tokio::test]
async fn init_then_load_roundtrips_same_vault_id() {
    // A vault re-loads from disk with the same id (proves persistence, not just
    // in-memory state).
    let fs = Arc::new(InMemoryFs::new());

    let vault1 = Vault::init(Arc::clone(&fs), author(1)).await.unwrap();
    let vault_id = vault1.vault_id();
    drop(vault1);

    let vault2 = Vault::load(Arc::clone(&fs), author(1)).await.unwrap();
    assert_eq!(vault2.vault_id(), vault_id);
}

#[tokio::test]
async fn adopt_vault_id_rewrites_metadata_and_persists() {
    // Pairing-time vault-id adoption persists across a reload.
    let fs = Arc::new(InMemoryFs::new());

    let mut vault = Vault::init(Arc::clone(&fs), author(1)).await.unwrap();
    let original_id = vault.vault_id();
    let adopted_id = VaultId::from(0xdeadbeefcafef00du64);
    assert_ne!(original_id, adopted_id, "test ids must differ");

    vault.adopt_vault_id(adopted_id).await.unwrap();

    // In-memory id reflects the adoption.
    assert_eq!(vault.vault_id(), adopted_id);

    // Reloading from disk reads the adopted id — proves persistence.
    drop(vault);
    let reloaded = Vault::load(Arc::clone(&fs), author(1)).await.unwrap();
    assert_eq!(reloaded.vault_id(), adopted_id);
}

#[tokio::test]
async fn adopt_vault_id_preserves_format_version() {
    // Adoption must not change the format version (migration safety).
    let fs = Arc::new(InMemoryFs::new());

    let mut vault = Vault::init(Arc::clone(&fs), author(1)).await.unwrap();
    vault
        .adopt_vault_id(VaultId::from(0x1111222233334444u64))
        .await
        .unwrap();

    let bytes = fs.read(METADATA_FILE).await.unwrap();
    let meta = String::from_utf8(bytes).unwrap();
    assert!(
        meta.contains("version = 1"),
        "adoption must not change the format version, got: {}",
        meta
    );
}

#[tokio::test]
async fn adopt_vault_id_same_id_is_noop() {
    // Adopting the id the vault already has is a no-op.
    let fs = Arc::new(InMemoryFs::new());

    let mut vault = Vault::init(Arc::clone(&fs), author(1)).await.unwrap();
    let current = vault.vault_id();

    vault.adopt_vault_id(current).await.unwrap();
    assert_eq!(vault.vault_id(), current);
}

#[tokio::test]
async fn legacy_vault_migration_generates_vault_id() {
    // A v0 legacy vault (.sync/ exists but no metadata.toml) migrates to v1 on
    // load, generating a vault id.
    let fs = Arc::new(InMemoryFs::new());

    // Simulate a legacy vault: .sync/ exists but no metadata.toml.
    fs.mkdir(SYNC_DIR).await.unwrap();
    fs.mkdir(&format!("{}/documents", SYNC_DIR)).await.unwrap();

    // Load runs the v0→v1 migration.
    let vault = Vault::load(Arc::clone(&fs), author(1)).await.unwrap();

    // metadata.toml now exists with version 1 and the vault's id.
    let bytes = fs.read(METADATA_FILE).await.unwrap();
    let meta = String::from_utf8(bytes).unwrap();
    assert!(meta.contains("version = 1"));
    assert!(meta.contains(&vault.vault_id().to_string()));
}

#[tokio::test]
async fn migration_is_idempotent() {
    // A second load after migration keeps the same id (does not regenerate).
    let fs = Arc::new(InMemoryFs::new());

    fs.mkdir(SYNC_DIR).await.unwrap();
    fs.mkdir(&format!("{}/documents", SYNC_DIR)).await.unwrap();

    let vault1 = Vault::load(Arc::clone(&fs), author(1)).await.unwrap();
    let vault_id = vault1.vault_id();
    drop(vault1);

    let vault2 = Vault::load(Arc::clone(&fs), author(1)).await.unwrap();
    assert_eq!(vault2.vault_id(), vault_id);
}

#[tokio::test]
async fn version_too_new_returns_error() {
    // A future-version vault is rejected on load (hard-fail contract).
    let fs = InMemoryFs::new();
    fs.mkdir(SYNC_DIR).await.unwrap();
    fs.mkdir(&format!("{}/documents", SYNC_DIR)).await.unwrap();

    // Write metadata with a future version.
    let meta = format!("version = 99\nvault_id = \"{}\"\n", VaultId::generate());
    fs.write(METADATA_FILE, meta.as_bytes()).await.unwrap();

    let result = Vault::load(fs, author(1)).await;
    let err = result.err().expect("should fail with version too new");
    assert!(
        err.to_string().contains("newer than supported"),
        "Got: {}",
        err
    );
}

#[tokio::test]
async fn corrupt_metadata_returns_error() {
    // A corrupt metadata.toml is rejected with a CorruptMetadata error.
    let fs = InMemoryFs::new();
    fs.mkdir(SYNC_DIR).await.unwrap();
    fs.mkdir(&format!("{}/documents", SYNC_DIR)).await.unwrap();

    // Write garbage to metadata.toml.
    fs.write(METADATA_FILE, b"not valid toml {{{{")
        .await
        .unwrap();

    let result = Vault::load(fs, author(1)).await;
    let err = result.err().expect("should fail with corrupt metadata");
    assert!(err.to_string().contains("Corrupt metadata"), "Got: {}", err);
}
