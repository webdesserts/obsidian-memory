//! Handshake protocol messages.
//!
//! Re-exports from sync-core for backwards compatibility.
//! The canonical implementation is in sync-core::handshake.

pub use sync_core::{is_likely_handshake, HandshakeMessage, MAX_MESSAGE_SIZE};
