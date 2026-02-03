//! SWIM protocol message types.
//!
//! All messages are sent as JSON over WebSocket for cross-platform compatibility
//! (both native Rust and browser WASM).

use crate::PeerId;
use serde::{Deserialize, Serialize};

/// Information about a peer in the mesh.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct PeerInfo {
    /// Peer's unique identifier
    pub peer_id: PeerId,
    /// Advertised address for incoming connections (None = client-only)
    pub address: Option<String>,
}

impl PeerInfo {
    /// Create a new PeerInfo.
    pub fn new(peer_id: PeerId, address: Option<String>) -> Self {
        Self { peer_id, address }
    }

    /// Create a client-only peer (no address).
    pub fn client_only(peer_id: PeerId) -> Self {
        Self {
            peer_id,
            address: None,
        }
    }
}

/// SWIM protocol messages.
///
/// These are sent between peers for failure detection and gossip dissemination.
/// All messages can carry piggybacked gossip updates for efficient propagation.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SwimMessage {
    /// Periodic health check with piggybacked gossip.
    Ping {
        /// Sequence number for matching acks
        seq: u64,
        /// Piggybacked gossip updates
        gossip: Vec<GossipUpdate>,
    },

    /// Response to ping with piggybacked gossip.
    Ack {
        /// Sequence number matching the ping
        seq: u64,
        /// Piggybacked gossip updates
        gossip: Vec<GossipUpdate>,
    },

    /// Indirect ping request - ask recipient to ping target on our behalf.
    PingReq {
        /// Target peer to ping
        target: PeerId,
        /// Sequence number for tracking
        seq: u64,
    },

    /// Response to PingReq - whether target responded.
    PingReqAck {
        /// Target peer that was pinged
        target: PeerId,
        /// Sequence number matching the request
        seq: u64,
        /// Whether target responded to the indirect ping
        alive: bool,
    },

    /// Buddy system: request verification of suspected peer.
    BuddyRequest {
        /// Suspected peer to verify
        target: PeerId,
    },

    /// Buddy system: verification result.
    BuddyResponse {
        /// Peer that was verified
        target: PeerId,
        /// Whether peer responded
        alive: bool,
    },
}

impl SwimMessage {
    /// Create a ping message with gossip updates.
    pub fn ping(seq: u64, gossip: Vec<GossipUpdate>) -> Self {
        Self::Ping { seq, gossip }
    }

    /// Create an ack message with gossip updates.
    pub fn ack(seq: u64, gossip: Vec<GossipUpdate>) -> Self {
        Self::Ack { seq, gossip }
    }

    /// Create a PingReq (indirect ping request).
    pub fn ping_req(target: PeerId, seq: u64) -> Self {
        Self::PingReq { target, seq }
    }

    /// Create a PingReqAck response.
    pub fn ping_req_ack(target: PeerId, seq: u64, alive: bool) -> Self {
        Self::PingReqAck { target, seq, alive }
    }

    /// Create a buddy request.
    pub fn buddy_request(target: PeerId) -> Self {
        Self::BuddyRequest { target }
    }

    /// Create a buddy response.
    pub fn buddy_response(target: PeerId, alive: bool) -> Self {
        Self::BuddyResponse { target, alive }
    }

    /// Serialize to JSON bytes.
    pub fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("SwimMessage serialization should not fail")
    }

    /// Try to parse from JSON bytes.
    pub fn from_json(data: &[u8]) -> Option<Self> {
        serde_json::from_slice(data).ok()
    }

    /// Check if data looks like a SWIM message (starts with '{').
    pub fn is_likely_swim_message(data: &[u8]) -> bool {
        data.first() == Some(&b'{')
    }

    /// Extract gossip updates from this message (if it carries any).
    pub fn gossip(&self) -> &[GossipUpdate] {
        match self {
            Self::Ping { gossip, .. } | Self::Ack { gossip, .. } => gossip,
            _ => &[],
        }
    }
}

/// Gossip update for membership state changes.
///
/// These are piggybacked on Ping/Ack messages for efficient dissemination.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum GossipUpdate {
    /// Peer is alive (join or refute suspicion).
    Alive {
        /// Peer information
        peer: PeerInfo,
        /// Incarnation number (higher refutes suspicion)
        incarnation: u64,
    },

    /// Peer is suspected (might be dead).
    Suspect {
        /// Peer's ID
        peer_id: PeerId,
        /// Incarnation number at time of suspicion
        incarnation: u64,
    },

    /// Peer confirmed dead (failed to refute suspicion).
    Dead {
        /// Peer's ID
        peer_id: PeerId,
    },

    /// Peer explicitly removed (collective forgetting).
    ///
    /// Unlike Dead, Removed means the peer was intentionally removed from the mesh.
    /// Don't attempt to reconnect to removed peers.
    Removed {
        /// Peer's ID
        peer_id: PeerId,
    },
}

impl GossipUpdate {
    /// Create an Alive update.
    pub fn alive(peer: PeerInfo, incarnation: u64) -> Self {
        Self::Alive { peer, incarnation }
    }

    /// Create a Suspect update.
    pub fn suspect(peer_id: PeerId, incarnation: u64) -> Self {
        Self::Suspect {
            peer_id,
            incarnation,
        }
    }

    /// Create a Dead update.
    pub fn dead(peer_id: PeerId) -> Self {
        Self::Dead { peer_id }
    }

    /// Create a Removed update.
    pub fn removed(peer_id: PeerId) -> Self {
        Self::Removed { peer_id }
    }

    /// Get the peer ID this update is about.
    pub fn peer_id(&self) -> PeerId {
        match self {
            Self::Alive { peer, .. } => peer.peer_id,
            Self::Suspect { peer_id, .. } => *peer_id,
            Self::Dead { peer_id } => *peer_id,
            Self::Removed { peer_id } => *peer_id,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_peer_id() -> PeerId {
        "a1b2c3d4e5f67890".parse().unwrap()
    }

    fn test_peer_id_2() -> PeerId {
        "1234567890abcdef".parse().unwrap()
    }

    // ==================== SwimMessage serialization ====================

    #[test]
    fn test_ping_serialization() {
        let msg = SwimMessage::ping(42, vec![]);
        let json = msg.to_json();
        let parsed = SwimMessage::from_json(&json).unwrap();
        assert_eq!(msg, parsed);
    }

    #[test]
    fn test_ping_with_gossip_serialization() {
        let peer = PeerInfo::new(test_peer_id(), Some("ws://localhost:8080".into()));
        let gossip = vec![GossipUpdate::alive(peer, 1)];
        let msg = SwimMessage::ping(42, gossip);

        let json = msg.to_json();
        let parsed = SwimMessage::from_json(&json).unwrap();
        assert_eq!(msg, parsed);
    }

    #[test]
    fn test_ack_serialization() {
        let msg = SwimMessage::ack(42, vec![]);
        let json = msg.to_json();
        let parsed = SwimMessage::from_json(&json).unwrap();
        assert_eq!(msg, parsed);
    }

    #[test]
    fn test_ping_req_serialization() {
        let msg = SwimMessage::ping_req(test_peer_id(), 99);
        let json = msg.to_json();
        let parsed = SwimMessage::from_json(&json).unwrap();
        assert_eq!(msg, parsed);
    }

    #[test]
    fn test_ping_req_ack_serialization() {
        let msg = SwimMessage::ping_req_ack(test_peer_id(), 99, true);
        let json = msg.to_json();
        let parsed = SwimMessage::from_json(&json).unwrap();
        assert_eq!(msg, parsed);
    }

    #[test]
    fn test_buddy_request_serialization() {
        let msg = SwimMessage::buddy_request(test_peer_id());
        let json = msg.to_json();
        let parsed = SwimMessage::from_json(&json).unwrap();
        assert_eq!(msg, parsed);
    }

    #[test]
    fn test_buddy_response_serialization() {
        let msg = SwimMessage::buddy_response(test_peer_id(), false);
        let json = msg.to_json();
        let parsed = SwimMessage::from_json(&json).unwrap();
        assert_eq!(msg, parsed);
    }

    // ==================== GossipUpdate serialization ====================

    #[test]
    fn test_gossip_alive_serialization() {
        let peer = PeerInfo::new(test_peer_id(), Some("ws://192.168.1.1:8080".into()));
        let gossip = GossipUpdate::alive(peer, 5);

        let json = serde_json::to_vec(&gossip).unwrap();
        let parsed: GossipUpdate = serde_json::from_slice(&json).unwrap();
        assert_eq!(gossip, parsed);
    }

    #[test]
    fn test_gossip_alive_client_only_serialization() {
        let peer = PeerInfo::client_only(test_peer_id());
        let gossip = GossipUpdate::alive(peer.clone(), 1);

        let json = serde_json::to_vec(&gossip).unwrap();
        let parsed: GossipUpdate = serde_json::from_slice(&json).unwrap();

        if let GossipUpdate::Alive { peer: p, .. } = parsed {
            assert!(p.address.is_none());
        } else {
            panic!("Expected Alive");
        }
    }

    #[test]
    fn test_gossip_suspect_serialization() {
        let gossip = GossipUpdate::suspect(test_peer_id(), 3);

        let json = serde_json::to_vec(&gossip).unwrap();
        let parsed: GossipUpdate = serde_json::from_slice(&json).unwrap();
        assert_eq!(gossip, parsed);
    }

    #[test]
    fn test_gossip_dead_serialization() {
        let gossip = GossipUpdate::dead(test_peer_id());

        let json = serde_json::to_vec(&gossip).unwrap();
        let parsed: GossipUpdate = serde_json::from_slice(&json).unwrap();
        assert_eq!(gossip, parsed);
    }

    #[test]
    fn test_gossip_removed_serialization() {
        let gossip = GossipUpdate::removed(test_peer_id());

        let json = serde_json::to_vec(&gossip).unwrap();
        let parsed: GossipUpdate = serde_json::from_slice(&json).unwrap();
        assert_eq!(gossip, parsed);
    }

    // ==================== Wire format compatibility ====================

    #[test]
    fn test_ping_wire_format() {
        let msg = SwimMessage::ping(1, vec![]);
        let json = String::from_utf8(msg.to_json()).unwrap();

        // Should use camelCase and have type field
        assert!(json.contains("\"type\":\"ping\""));
        assert!(json.contains("\"seq\":1"));
        assert!(json.contains("\"gossip\":[]"));
    }

    #[test]
    fn test_gossip_wire_format() {
        let peer = PeerInfo::new(test_peer_id(), Some("ws://localhost:8080".into()));
        let gossip = GossipUpdate::alive(peer, 1);
        let json = serde_json::to_string(&gossip).unwrap();

        // Should use camelCase
        assert!(json.contains("\"type\":\"alive\""));
        assert!(json.contains("\"peerId\":"));
        assert!(json.contains("\"incarnation\":1"));
    }

    #[test]
    fn test_is_likely_swim_message() {
        let msg = SwimMessage::ping(1, vec![]);
        assert!(SwimMessage::is_likely_swim_message(&msg.to_json()));

        // Bincode sync messages don't start with '{'
        let bincode_data = vec![0x00, 0x01, 0x02, 0x03];
        assert!(!SwimMessage::is_likely_swim_message(&bincode_data));

        assert!(!SwimMessage::is_likely_swim_message(&[]));
    }

    #[test]
    fn test_gossip_peer_id_extraction() {
        let peer_id = test_peer_id();
        let peer_id_2 = test_peer_id_2();

        let alive = GossipUpdate::alive(PeerInfo::client_only(peer_id), 1);
        assert_eq!(alive.peer_id(), peer_id);

        let suspect = GossipUpdate::suspect(peer_id_2, 1);
        assert_eq!(suspect.peer_id(), peer_id_2);

        let dead = GossipUpdate::dead(peer_id);
        assert_eq!(dead.peer_id(), peer_id);

        let removed = GossipUpdate::removed(peer_id_2);
        assert_eq!(removed.peer_id(), peer_id_2);
    }

    #[test]
    fn test_message_gossip_extraction() {
        let gossip = vec![
            GossipUpdate::alive(PeerInfo::client_only(test_peer_id()), 1),
            GossipUpdate::suspect(test_peer_id_2(), 2),
        ];

        let ping = SwimMessage::ping(1, gossip.clone());
        assert_eq!(ping.gossip(), &gossip);

        let ack = SwimMessage::ack(1, gossip.clone());
        assert_eq!(ack.gossip(), &gossip);

        // Messages without gossip return empty slice
        let ping_req = SwimMessage::ping_req(test_peer_id(), 1);
        assert!(ping_req.gossip().is_empty());

        let buddy_req = SwimMessage::buddy_request(test_peer_id());
        assert!(buddy_req.gossip().is_empty());
    }

    #[test]
    fn test_invalid_json_returns_none() {
        assert!(SwimMessage::from_json(b"not json").is_none());
        assert!(SwimMessage::from_json(b"{}").is_none()); // Missing required fields
        assert!(SwimMessage::from_json(b"").is_none());
    }

    #[test]
    fn test_multiple_gossip_updates() {
        let peer1 = PeerInfo::new(test_peer_id(), Some("ws://a:8080".into()));
        let peer2 = PeerInfo::new(test_peer_id_2(), None);

        let gossip = vec![
            GossipUpdate::alive(peer1, 1),
            GossipUpdate::alive(peer2, 1),
            GossipUpdate::suspect(test_peer_id(), 2),
        ];

        let msg = SwimMessage::ping(100, gossip);
        let json = msg.to_json();
        let parsed = SwimMessage::from_json(&json).unwrap();

        if let SwimMessage::Ping { gossip, .. } = parsed {
            assert_eq!(gossip.len(), 3);
        } else {
            panic!("Expected Ping");
        }
    }
}
