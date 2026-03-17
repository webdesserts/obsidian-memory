//! Pairing protocol message types.
//!
//! These types define the messages exchanged during the device pairing flow.
//! The actual transport and confirmation code exchange depend on iroh networking
//! from Effort 4 — this module defines the data structures only.

use serde::{Deserialize, Serialize};

use crate::peer_id::PeerId;

/// Sent by a device that wants to pair with another vault.
///
/// The `code_hash` is a SHA-256 of the 6-digit confirmation code displayed
/// to the user. The receiving device verifies by hashing the code it shows
/// and comparing hashes before prompting for user confirmation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PairingRequest {
    /// The requesting device's ed25519 public key.
    pub node_id: PeerId,
    /// Human-readable name of the requesting device (e.g., "MacBook Pro").
    pub device_name: String,
    /// SHA-256 of the 6-digit confirmation code shown to the user.
    pub code_hash: [u8; 32],
}

/// Sent by the pairing acceptor to confirm the pairing.
///
/// The `hmac` proves knowledge of the confirmation code without transmitting
/// it directly: `HMAC-SHA256(key=code, msg=node_id_bytes)`.
/// The acceptor also shares the gossip topic and relay URLs so the new peer
/// can join the sync network immediately after pairing completes.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PairingConfirmation {
    /// The accepting device's ed25519 public key.
    pub node_id: PeerId,
    /// Human-readable name of the accepting device.
    pub device_name: String,
    /// HMAC-SHA256(key=code_bytes, msg=node_id_bytes) proving code knowledge.
    pub hmac: [u8; 32],
    /// The gossip topic ID for this vault (iroh-gossip topic).
    pub vault_topic: [u8; 32],
    /// Relay URLs for NAT traversal (empty if direct connection is available).
    pub relay_urls: Vec<String>,
}
