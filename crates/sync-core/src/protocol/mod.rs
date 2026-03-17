//! Protocol definitions for P2P sync.
//!
//! This module defines the wire protocol for peer communication:
//! - Versioned handshake for initial connection
//! - Bincode-encoded sync messages (SyncMessage enum)

pub mod handshake;

pub use handshake::{Handshake, HandshakeRole, MAX_MESSAGE_SIZE, PROTOCOL_VERSION};
