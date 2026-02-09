//! Wire protocol envelope types for peer-to-peer messages.
//!
//! These types define the JSON wire format for gossip and sync messages
//! exchanged between peers. They replace ad-hoc `serde_json::json!()`
//! construction and manual JSON parsing with typed structs.
//!
//! The handshake message is handled separately in [`super::handshake`]
//! since it operates at the connection level, not the message level.

use crate::swim::GossipUpdate;
use serde::{Deserialize, Serialize};

/// A standalone gossip message containing SWIM membership updates.
///
/// Wire format: `{"type":"gossip","updates":[...]}`
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GossipMessage {
    #[serde(rename = "type")]
    msg_type: String,
    pub updates: Vec<GossipUpdate>,
}

impl GossipMessage {
    /// Create a new gossip message wrapping the given updates.
    pub fn new(updates: Vec<GossipUpdate>) -> Self {
        Self {
            msg_type: "gossip".to_string(),
            updates,
        }
    }

    /// Serialize to JSON bytes.
    pub fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("GossipMessage serialization should not fail")
    }

    /// Try to parse from JSON bytes.
    ///
    /// Returns `None` for non-JSON input or if the `type` field isn't `"gossip"`.
    pub fn from_json(data: &[u8]) -> Option<Self> {
        let msg: Self = serde_json::from_slice(data).ok()?;
        if msg.msg_type == "gossip" {
            Some(msg)
        } else {
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::swim::PeerInfo;
    use crate::PeerId;

    fn test_peer_id() -> PeerId {
        "a1b2c3d4e5f67890".parse().unwrap()
    }

    fn sample_updates() -> Vec<GossipUpdate> {
        let peer = PeerInfo::new(test_peer_id(), Some("ws://10.0.0.1:8080".into()));
        vec![GossipUpdate::alive(peer, 1)]
    }

    // ==================== GossipMessage ====================

    #[test]
    fn test_gossip_message_roundtrip() {
        let msg = GossipMessage::new(sample_updates());
        let json = msg.to_json();
        let parsed = GossipMessage::from_json(&json).unwrap();
        assert_eq!(msg, parsed);
    }

    #[test]
    fn test_gossip_message_wire_format() {
        let msg = GossipMessage::new(sample_updates());
        let json = String::from_utf8(msg.to_json()).unwrap();
        assert!(json.contains("\"type\":\"gossip\""));
        assert!(json.contains("\"updates\":["));
    }

    #[test]
    fn test_gossip_message_empty_updates() {
        let msg = GossipMessage::new(vec![]);
        let json = msg.to_json();
        let parsed = GossipMessage::from_json(&json).unwrap();
        assert!(parsed.updates.is_empty());
    }

    #[test]
    fn test_gossip_message_wrong_type() {
        let json = br#"{"type":"sync","updates":[]}"#;
        assert!(GossipMessage::from_json(json).is_none());
    }

    #[test]
    fn test_gossip_message_invalid_json() {
        assert!(GossipMessage::from_json(b"not json").is_none());
        assert!(GossipMessage::from_json(b"").is_none());
        assert!(GossipMessage::from_json(&[0xFF, 0xFE]).is_none());
    }
}
