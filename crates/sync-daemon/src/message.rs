//! Handshake protocol messages.
//!
//! The handshake is JSON sent as binary WebSocket frames (UTF-8 bytes),
//! matching the plugin's protocol exactly.

use serde::{Deserialize, Serialize};

/// Maximum message size (50MB) to prevent memory exhaustion from malicious peers.
pub const MAX_MESSAGE_SIZE: usize = 50 * 1024 * 1024;

/// Handshake message exchanged when a peer connects.
///
/// Sent as binary WebSocket frame containing UTF-8 JSON.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HandshakeMessage {
    /// Always "handshake"
    #[serde(rename = "type")]
    pub msg_type: String,

    /// The peer's unique identifier
    #[serde(rename = "peerId")]
    pub peer_id: String,

    /// Role in the connection: "server" or "client"
    pub role: String,
}

impl HandshakeMessage {
    /// Create a new handshake message.
    pub fn new(peer_id: &str, role: &str) -> Self {
        Self {
            msg_type: "handshake".to_string(),
            peer_id: peer_id.to_string(),
            role: role.to_string(),
        }
    }

    /// Serialize to UTF-8 JSON bytes for sending as binary WebSocket frame.
    pub fn to_binary(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("HandshakeMessage serialization should not fail")
    }

    /// Try to parse a handshake from binary data.
    ///
    /// Returns None if the data is not valid UTF-8 JSON or not a handshake message.
    pub fn from_binary(data: &[u8]) -> Option<Self> {
        // Try to parse as UTF-8 string first
        let text = std::str::from_utf8(data).ok()?;

        // Try to parse as JSON
        let msg: Self = serde_json::from_str(text).ok()?;

        // Verify it's actually a handshake
        if msg.msg_type == "handshake" {
            Some(msg)
        } else {
            None
        }
    }
}

/// Quick check if data looks like a JSON handshake (starts with '{').
///
/// This is a fast pre-check before attempting full JSON parsing.
/// Binary sync messages (bincode) won't start with '{'.
pub fn is_likely_handshake(data: &[u8]) -> bool {
    // JSON objects start with '{', bincode messages typically don't
    data.first() == Some(&b'{')
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_handshake_roundtrip() {
        let msg = HandshakeMessage::new("test-peer-123", "server");
        let binary = msg.to_binary();
        let parsed = HandshakeMessage::from_binary(&binary).unwrap();

        assert_eq!(parsed.msg_type, "handshake");
        assert_eq!(parsed.peer_id, "test-peer-123");
        assert_eq!(parsed.role, "server");
    }

    #[test]
    fn test_is_likely_handshake() {
        let handshake = HandshakeMessage::new("peer", "client").to_binary();
        assert!(is_likely_handshake(&handshake));

        // Bincode sync messages don't start with '{'
        let bincode_data = vec![0x00, 0x01, 0x02, 0x03];
        assert!(!is_likely_handshake(&bincode_data));
    }

    #[test]
    fn test_invalid_json_returns_none() {
        let invalid = b"not json at all";
        assert!(HandshakeMessage::from_binary(invalid).is_none());
    }

    #[test]
    fn test_non_handshake_json_returns_none() {
        let other_json = b"{\"type\": \"other\", \"data\": 123}";
        assert!(HandshakeMessage::from_binary(other_json).is_none());
    }
}
