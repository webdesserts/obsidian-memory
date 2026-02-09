//! sync-daemon library: Exposes internal modules for testing.
//!
//! This is a thin library layer over the daemon components,
//! allowing integration tests to access internal types.

pub mod connection;
pub mod manager;
pub mod message;
pub mod native_fs;
pub mod outgoing;
pub mod persistence;
pub mod server;
pub mod watcher;

// Re-export key types for convenience
pub use connection::{ConnectionEvent, IncomingMessage, PeerConnection};
pub use manager::{ConnectionManager, ManagerEvent};
pub use message::{Handshake, HandshakeRole, MAX_MESSAGE_SIZE, PROTOCOL_VERSION};
pub use native_fs::NativeFs;
pub use outgoing::{OutgoingConnection, OutgoingState, ReconnectConfig, ReconnectState};
pub use persistence::{PeerStorage, PersistedPeer, PersistedPeers};
pub use server::{ServerEvent, WebSocketServer};
pub use watcher::{FileEvent, FileEventKind, FileWatcher};
