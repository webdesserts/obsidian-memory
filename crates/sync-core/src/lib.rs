//! sync-core: Shared Rust library for P2P vault synchronization using Loro CRDTs.
//!
//! This crate provides the core functionality for:
//! - Managing Loro documents for markdown notes
//! - Parsing/serializing markdown with frontmatter
//! - Sync protocol between peers
//! - FileSystem and SyncTransport trait abstractions

pub mod document;
pub mod events;
pub mod fs;
pub mod markdown;
pub mod peer_id;
pub mod sync;
pub mod sync_engine;
pub mod transport;
pub mod vault;

pub use document::NoteDocument;
pub use events::{EventBus, Subscription, SyncEvent};
pub use fs::{FileEntry, FileStat, FileSystem, InMemoryFs};
pub use peer_id::{PeerId, PeerIdError};
pub use sync::SyncMessage;
pub use transport::{PeerConnection, PeerInfo, SyncTransport};
pub use vault::Vault;
