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
use serde_json::Value;

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

/// A sync data envelope with optional piggybacked gossip updates.
///
/// Wire format: `{"type":"sync","data":[1,2,3,...],"gossip":[...]}`
///
/// The `data` field is a `Vec<u8>` that serializes as a JSON number array,
/// matching the TypeScript plugin's `Array.from(syncData)` convention.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SyncEnvelope {
    #[serde(rename = "type")]
    msg_type: String,
    pub data: Vec<u8>,
    #[serde(default)]
    pub gossip: Vec<GossipUpdate>,
}

impl SyncEnvelope {
    /// Create a new sync envelope with data and optional gossip updates.
    pub fn new(data: Vec<u8>, gossip: Vec<GossipUpdate>) -> Self {
        Self {
            msg_type: "sync".to_string(),
            data,
            gossip,
        }
    }

    /// Serialize to JSON bytes.
    pub fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("SyncEnvelope serialization should not fail")
    }

    /// Try to parse from JSON bytes.
    ///
    /// Returns `None` for non-JSON input or if the `type` field isn't `"sync"`.
    pub fn from_json(data: &[u8]) -> Option<Self> {
        let msg: Self = serde_json::from_slice(data).ok()?;
        if msg.msg_type == "sync" {
            Some(msg)
        } else {
            None
        }
    }
}

/// Parsed incoming peer message, routed by the `type` field.
///
/// This is a parsing-only type — callers construct [`GossipMessage`] or
/// [`SyncEnvelope`] directly when sending. Use `from_json()` to route
/// incoming bytes. Returns `None` for non-JSON or unknown types, allowing
/// callers to fall back to raw binary sync handling.
///
/// Handshake messages are intentionally excluded — they're handled at the
/// connection level before normal message flow begins.
#[derive(Debug)]
pub enum PeerMessage {
    Gossip(GossipMessage),
    Sync(SyncEnvelope),
}

impl PeerMessage {
    /// Try to parse and route an incoming message by its `type` field.
    ///
    /// Returns `None` for non-JSON input or unknown message types.
    pub fn from_json(data: &[u8]) -> Option<Self> {
        let value: Value = serde_json::from_slice(data).ok()?;
        let msg_type = value.get("type")?.as_str()?;

        match msg_type {
            "gossip" => {
                let msg: GossipMessage = serde_json::from_value(value).ok()?;
                Some(PeerMessage::Gossip(msg))
            }
            "sync" => {
                let msg: SyncEnvelope = serde_json::from_value(value).ok()?;
                Some(PeerMessage::Sync(msg))
            }
            _ => None,
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

    // ==================== SyncEnvelope ====================

    #[test]
    fn test_sync_envelope_roundtrip() {
        let msg = SyncEnvelope::new(vec![1, 2, 3, 4], sample_updates());
        let json = msg.to_json();
        let parsed = SyncEnvelope::from_json(&json).unwrap();
        assert_eq!(msg, parsed);
    }

    #[test]
    fn test_sync_envelope_wire_format() {
        let msg = SyncEnvelope::new(vec![10, 20], sample_updates());
        let json = String::from_utf8(msg.to_json()).unwrap();
        assert!(json.contains("\"type\":\"sync\""));
        assert!(json.contains("\"data\":[10,20]"));
        assert!(json.contains("\"gossip\":["));
    }

    #[test]
    fn test_sync_envelope_missing_gossip() {
        // Gossip field absent — should default to empty vec via #[serde(default)]
        let json = br#"{"type":"sync","data":[1,2,3]}"#;
        let parsed = SyncEnvelope::from_json(json).unwrap();
        assert_eq!(parsed.data, vec![1, 2, 3]);
        assert!(parsed.gossip.is_empty());
    }

    #[test]
    fn test_sync_envelope_data_as_byte_array() {
        // Verify Vec<u8> round-trips as JSON number array (matching TS Array.from())
        let original_data: Vec<u8> = vec![0, 127, 255];
        let msg = SyncEnvelope::new(original_data.clone(), vec![]);
        let json = String::from_utf8(msg.to_json()).unwrap();
        assert!(json.contains("[0,127,255]"));

        let parsed = SyncEnvelope::from_json(json.as_bytes()).unwrap();
        assert_eq!(parsed.data, original_data);
    }

    #[test]
    fn test_sync_envelope_wrong_type() {
        let json = br#"{"type":"gossip","data":[1],"gossip":[]}"#;
        assert!(SyncEnvelope::from_json(json).is_none());
    }

    // ==================== PeerMessage ====================

    #[test]
    fn test_peer_message_routes_gossip() {
        let gossip = GossipMessage::new(sample_updates());
        let json = gossip.to_json();
        let parsed = PeerMessage::from_json(&json);
        assert!(matches!(parsed, Some(PeerMessage::Gossip(_))));
    }

    #[test]
    fn test_peer_message_routes_sync() {
        let envelope = SyncEnvelope::new(vec![1, 2, 3], vec![]);
        let json = envelope.to_json();
        let parsed = PeerMessage::from_json(&json);
        assert!(matches!(parsed, Some(PeerMessage::Sync(_))));
    }

    #[test]
    fn test_peer_message_non_json_returns_none() {
        assert!(PeerMessage::from_json(&[0x00, 0x01, 0x02]).is_none());
        assert!(PeerMessage::from_json(b"raw binary data").is_none());
    }

    #[test]
    fn test_peer_message_unknown_type_returns_none() {
        // Handshakes are handled at the connection level, not here
        let json = br#"{"type":"handshake","peerId":"abc","role":"server"}"#;
        assert!(PeerMessage::from_json(json).is_none());
    }

    #[test]
    fn test_peer_message_empty_returns_none() {
        assert!(PeerMessage::from_json(b"").is_none());
    }
}
