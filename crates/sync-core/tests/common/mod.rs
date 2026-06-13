//! Shared helpers for sync-core integration tests.
//!
//! Each integration-test binary (`tests/*.rs`) compiles this module
//! independently and uses only the subset of helpers it needs, so
//! `#![allow(dead_code)]` suppresses the per-binary "never used" warnings for the
//! helpers a given binary doesn't reach.
//!
//! These drive the sync protocol through the public `Vault` API only — no
//! reach-ins to `pub(crate)` internals — so they're reusable across the
//! sync-engine and vault test surfaces. Each helper hands back a retained
//! `Arc<InMemoryFs>` so a test can make post-init edits through that handle
//! rather than the vault's internal `fs` field.

#![allow(dead_code)]

use std::sync::Arc;

use sync_core::fs::{FileSystem, InMemoryFs};
use sync_core::{PeerId, Vault};

/// A deterministic `PeerId` built from a single byte, e.g. `author(1)` /
/// `author(2)` for the two sides of a two-vault handshake.
pub fn author(n: u8) -> PeerId {
    PeerId::from_bytes([n; 32])
}

/// Build two in-memory vaults, seeding each with `(path, content)` files before
/// `Vault::init` indexes them. Vault A is authored by `author(1)`, vault B by
/// `author(2)`.
///
/// Returns the vaults plus their retained `Arc<InMemoryFs>` handles
/// `(vault_a, vault_b, fs_a, fs_b)`. The handles share storage with the vaults
/// (the vault is `Vault<Arc<InMemoryFs>>`), so a test can write/delete through
/// the handle to simulate an external edit and then call `on_file_changed` /
/// `Vault::load` to observe how the vault reacts.
pub async fn two_vaults(
    files_a: &[(&str, &str)],
    files_b: &[(&str, &str)],
) -> (
    Vault<Arc<InMemoryFs>>,
    Vault<Arc<InMemoryFs>>,
    Arc<InMemoryFs>,
    Arc<InMemoryFs>,
) {
    let fs_a = Arc::new(InMemoryFs::new());
    let fs_b = Arc::new(InMemoryFs::new());

    for (path, content) in files_a {
        fs_a.write(path, content.as_bytes()).await.unwrap();
    }
    for (path, content) in files_b {
        fs_b.write(path, content.as_bytes()).await.unwrap();
    }

    let vault_a = Vault::init(Arc::clone(&fs_a), author(1)).await.unwrap();
    let vault_b = Vault::init(Arc::clone(&fs_b), author(2)).await.unwrap();
    (vault_a, vault_b, fs_a, fs_b)
}

/// Build a single in-memory vault, seeding `(path, content)` files before
/// `Vault::init` indexes them. Authored by `author(1)`.
///
/// Returns the vault plus its retained `Arc<InMemoryFs>` handle so a test can
/// write/delete through the handle to simulate an external edit and then call
/// `on_file_changed` / `Vault::load` to observe how the vault reacts. This is the
/// single-vault counterpart to `two_vaults` for the many vault tests that exercise
/// reconcile / quarantine / register / delete / rename against one vault.
pub async fn one_vault(files: &[(&str, &str)]) -> (Vault<Arc<InMemoryFs>>, Arc<InMemoryFs>) {
    let fs = Arc::new(InMemoryFs::new());
    for (path, content) in files {
        fs.write(path, content.as_bytes()).await.unwrap();
    }
    let vault = Vault::init(Arc::clone(&fs), author(1)).await.unwrap();
    (vault, fs)
}

/// Drop `vault` and load a fresh one over the same filesystem, simulating a
/// process restart from a cold cache (the `.loro`/registry persist on disk while
/// the in-memory document cache starts empty). Authored by `author(1)` to match
/// `one_vault`. Used by the "survives reload" persistence and migration tests.
pub async fn reload(
    vault: Vault<Arc<InMemoryFs>>,
    fs: &Arc<InMemoryFs>,
) -> Vault<Arc<InMemoryFs>> {
    drop(vault);
    Vault::load(Arc::clone(fs), author(1)).await.unwrap()
}

/// Drive the complete three-message sync handshake with `a` as the initiator
/// and return `(modified_a, modified_b)` — the paths each vault received.
///
/// The exchange is: A sends a SyncRequest → B answers with a SyncExchange (its
/// response plus its own request) → A applies it and sends back a final
/// SyncResponse → B applies that and the handshake terminates. The protocol
/// always produces a final SyncResponse, so the `.unwrap()` chain is exact;
/// getting this order wrong is the latent "pass-by-luck" risk this helper
/// removes. Tests that assert on the intermediate messages keep the manual form.
pub async fn full_sync<F: FileSystem>(a: &Vault<F>, b: &Vault<F>) -> (Vec<String>, Vec<String>) {
    let request = a.prepare_sync_request().await.unwrap();
    let (exchange, _) = b.process_sync_message(&request).await.unwrap();
    let (final_response, modified_a) = a.process_sync_message(&exchange.unwrap()).await.unwrap();
    let (_, modified_b) = b
        .process_sync_message(&final_response.unwrap())
        .await
        .unwrap();
    (modified_a, modified_b)
}
