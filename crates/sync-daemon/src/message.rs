//! Protocol message types.
//!
//! Re-exports from sync-core's protocol module.

pub use sync_core::protocol::{
    GossipMessage, Handshake, HandshakeRole, PeerMessage, SyncEnvelope, MAX_MESSAGE_SIZE,
    PROTOCOL_VERSION,
};
