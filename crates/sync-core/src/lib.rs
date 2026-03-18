// Deny holding RefCell borrows across await points - causes WASM panics
#![deny(clippy::await_holding_refcell_ref)]

//! sync-core: Shared Rust library for P2P vault synchronization using Loro CRDTs.
//!
//! This crate provides the core functionality for:
//! - Managing Loro documents for markdown notes
//! - Parsing/serializing markdown with frontmatter
//! - Sync protocol between peers
//! - FileSystem trait abstraction

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

pub use allowlist::{AllowedPeer, AllowlistError, AllowlistStorage};
pub use document::NoteDocument;
pub use events::{EventBus, Subscription, SyncEvent};
pub use fs::{FileEntry, FileStat, FileSystem, InMemoryFs};
pub use key_storage::{KeyStorage, KeyStorageError};
pub use pairing::{
    PairingChallenge, PairingHello, PairingResponse, PairingResult, PairingSession,
    compute_hmac, generate_pairing_code, hash_code, verify_hmac,
};
pub use peer_id::{PeerId, PeerIdError, VaultId};
pub use peers::{PeerEntry, PeerRegistry, PeerState};
pub use sync::SyncMessage;
pub use vault::{SyncMetadata, Vault};
