//! sync-daemon library: Exposes internal modules for testing.
//!
//! This is a thin library layer over the daemon components,
//! allowing integration tests to access internal types.

pub mod connection;
pub mod message;
pub mod native_fs;
pub mod server;
pub mod watcher;

// Re-export key types for convenience
pub use connection::{ConnectionEvent, IncomingMessage, PeerConnection};
pub use message::{HandshakeMessage, MAX_MESSAGE_SIZE};
pub use native_fs::NativeFs;
pub use server::WebSocketServer;
pub use watcher::{FileEvent, FileEventKind, FileWatcher};
