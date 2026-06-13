// Deny holding RefCell borrows across await points - causes WASM panics
#![deny(clippy::await_holding_refcell_ref)]

//! sync-core: Shared Rust library for P2P vault synchronization using Loro CRDTs.
//!
//! This crate provides the core functionality for:
//! - Managing Loro documents for markdown notes
//! - Parsing/serializing markdown with frontmatter
//! - Sync protocol between peers
//! - FileSystem trait abstraction
//!
//! ## Feature gates
//!
//! - `cfg(feature = "native")` — code requiring iroh mDNS/sockets, tokio runtime, or
//!   OS-level APIs. These are unavailable in WASM or when building without the `native`
//!   feature (e.g. the Obsidian plugin via sync-wasm).
//! - `cfg(target_arch = "wasm32")` — WASM-specific runtime differences: `Rc`/`RefCell`
//!   instead of `Arc`/`RwLock`, and `?Send` async trait bounds. These gates exist
//!   because Cargo feature unification can't express "Send only when native feature is
//!   enabled" — the arch check is the only reliable way to vary trait bounds.

pub mod network;

pub mod allowlist;
pub mod document;
pub mod events;
pub mod fs;
pub mod key_storage;
pub mod markdown;
pub mod pairing;
pub mod peer_id;
pub mod peers;
pub mod sync;
pub mod sync_engine;
pub mod vault;

#[cfg(not(target_arch = "wasm32"))]
pub use allowlist::InMemoryAllowlist;
pub use allowlist::{AllowedPeer, AllowlistError, AllowlistStorage};
pub use document::NoteDocument;
pub use events::{EventBus, Subscription, SyncEvent};
pub use fs::{FileEntry, FileStat, FileSystem, InMemoryFs};
pub use key_storage::{KeyStorage, KeyStorageError};
pub use pairing::{
    PairingChallenge, PairingHello, PairingResponse, PairingResult, PairingSession, compute_hmac,
    generate_pairing_code, verify_hmac,
};
pub use peer_id::{PeerId, PeerIdError, VaultId};
pub use peers::{PeerEntry, PeerRegistry, PeerState};
pub use sync::SyncMessage;
pub use vault::{DebrisReport, DuplicateGroup, FolderDupGroup, Relic, SyncMetadata, Vault};

// Re-export the loro TreeID so consumers of the registry-debris API (DebrisReport et al.)
// can name node identities without taking a direct `loro` dependency.
pub use loro::TreeID;
