//! Protocol definitions for P2P sync.
//!
//! This module defines the wire protocol for peer communication:
//! - Versioned handshake for initial connection
//! - Message encoding (JSON vs Bincode detection)
//! - SWIM gossip messages (via swim module)

pub mod encoding;
pub mod envelope;
pub mod handshake;

pub use encoding::{detect_message_type, MessageType};
pub use envelope::GossipMessage;
pub use handshake::{Handshake, HandshakeRole, MAX_MESSAGE_SIZE, PROTOCOL_VERSION};
