//! SWIM (Scalable Weakly-consistent Infection-style Membership) protocol implementation.
//!
//! Custom SWIM implementation over WebSocket for P2P peer discovery and failure detection.
//! Same code runs in daemon (native) and plugin (WASM).
//!
//! # Protocol Overview
//!
//! **Failure Detection:**
//! 1. Each peer periodically pings a random other peer
//! 2. If no ack within timeout, send indirect ping (ask K other peers to ping target)
//! 3. If indirect ping also fails, mark target as "suspected"
//! 4. If suspicion timeout expires, mark as "dead"
//!
//! **Gossip Dissemination:**
//! - Piggyback membership updates on every ping/ack message
//! - Updates: `Alive`, `Suspect`, `Dead`, `Removed`
//! - Rapid convergence through infection-style spread

pub mod membership;
pub mod messages;

pub use membership::{Member, MemberState, MembershipList};
pub use messages::{GossipUpdate, PeerInfo, SwimMessage};
