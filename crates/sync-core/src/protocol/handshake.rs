//! Versioned handshake protocol.
//!
//! The handshake is sent immediately after WebSocket connection is established.
//! It includes:
//! - Protocol version for forward compatibility
//! - Peer ID (unique identifier)
//! - Role (server or client)
//! - Address for incoming connections (None for client-only)

use crate::PeerId;
use serde::{Deserialize, Serialize};

/// Current protocol version.
///
/// Increment when making breaking changes to the protocol.
pub const PROTOCOL_VERSION: u32 = 1;

/// Role in the P2P mesh.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum HandshakeRole {
    /// Can accept incoming connections (daemons, desktop plugins)
    Server,
    /// Can only connect outgoing (iOS, NAT without port forwarding)
    Client,
}

impl HandshakeRole {
    /// Check if this role can accept incoming connections.
    pub fn is_server(&self) -> bool {
        matches!(self, Self::Server)
    }

    /// Check if this role is client-only.
    pub fn is_client(&self) -> bool {
        matches!(self, Self::Client)
    }
}

/// Versioned handshake message.
///
/// Sent immediately after WebSocket connection is established.
/// Both peers send their handshake, then continue with SWIM gossip.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct Handshake {
    /// Message type discriminator
    #[serde(rename = "type")]
    pub msg_type: String,
    /// Protocol version
    pub version: u32,
    /// Peer's unique identifier
    pub peer_id: PeerId,
    /// Role in the connection
    pub role: HandshakeRole,
    /// Advertised address for incoming connections (None = client-only)
    #[serde(skip_serializing_if = "Option::is_none")]
    pub address: Option<String>,
}

impl Handshake {
    /// Create a new handshake for a server-capable peer.
    pub fn server(peer_id: PeerId, address: String) -> Self {
        Self {
            msg_type: "handshake".to_string(),
            version: PROTOCOL_VERSION,
            peer_id,
            role: HandshakeRole::Server,
            address: Some(address),
        }
    }

    /// Create a new handshake for a client-only peer.
    pub fn client(peer_id: PeerId) -> Self {
        Self {
            msg_type: "handshake".to_string(),
            version: PROTOCOL_VERSION,
            peer_id,
            role: HandshakeRole::Client,
            address: None,
        }
    }

    /// Create a handshake with explicit parameters.
    pub fn new(peer_id: PeerId, role: HandshakeRole, address: Option<String>) -> Self {
        Self {
            msg_type: "handshake".to_string(),
            version: PROTOCOL_VERSION,
            peer_id,
            role,
            address,
        }
    }

    /// Serialize to JSON bytes.
    pub fn to_json(&self) -> Vec<u8> {
        serde_json::to_vec(self).expect("Handshake serialization should not fail")
    }

    /// Try to parse from JSON bytes.
    pub fn from_json(data: &[u8]) -> Option<Self> {
        let handshake: Self = serde_json::from_slice(data).ok()?;

        // Verify it's actually a handshake
        if handshake.msg_type == "handshake" {
            Some(handshake)
        } else {
            None
        }
    }

    /// Check if this is compatible with our version.
    ///
    /// Currently accepts any version (graceful degradation).
    /// Future versions may reject incompatible protocols.
    pub fn is_compatible(&self) -> bool {
        // Accept any version for now - we'll add strict checking later if needed
        true
    }

    /// Check if we should log a version mismatch warning.
    pub fn should_warn_version(&self) -> bool {
        self.version != PROTOCOL_VERSION
    }

    /// Check if this peer can accept incoming connections.
    pub fn is_server(&self) -> bool {
        self.role.is_server() && self.address.is_some()
    }

    /// Check if this peer is client-only.
    pub fn is_client_only(&self) -> bool {
        self.role.is_client() || self.address.is_none()
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

    // ==================== Construction ====================

    #[test]
    fn test_server_handshake() {
        let hs = Handshake::server(test_peer_id(), "ws://192.168.1.1:8080".into());

        assert_eq!(hs.msg_type, "handshake");
        assert_eq!(hs.version, PROTOCOL_VERSION);
        assert_eq!(hs.peer_id, test_peer_id());
        assert_eq!(hs.role, HandshakeRole::Server);
        assert_eq!(hs.address, Some("ws://192.168.1.1:8080".into()));
    }

    #[test]
    fn test_client_handshake() {
        let hs = Handshake::client(test_peer_id());

        assert_eq!(hs.msg_type, "handshake");
        assert_eq!(hs.version, PROTOCOL_VERSION);
        assert_eq!(hs.peer_id, test_peer_id());
        assert_eq!(hs.role, HandshakeRole::Client);
        assert!(hs.address.is_none());
    }

    // ==================== Serialization ====================

    #[test]
    fn test_server_roundtrip() {
        let hs = Handshake::server(test_peer_id(), "ws://localhost:8080".into());
        let json = hs.to_json();
        let parsed = Handshake::from_json(&json).unwrap();

        assert_eq!(hs, parsed);
    }

    #[test]
    fn test_client_roundtrip() {
        let hs = Handshake::client(test_peer_id());
        let json = hs.to_json();
        let parsed = Handshake::from_json(&json).unwrap();

        assert_eq!(hs, parsed);
    }

    #[test]
    fn test_wire_format_server() {
        let hs = Handshake::server(test_peer_id(), "ws://a:8080".into());
        let json = String::from_utf8(hs.to_json()).unwrap();

        assert!(json.contains("\"type\":\"handshake\""));
        assert!(json.contains("\"version\":1"));
        assert!(json.contains("\"peerId\":"));
        assert!(json.contains("\"role\":\"server\""));
        assert!(json.contains("\"address\":"));
    }

    #[test]
    fn test_wire_format_client() {
        let hs = Handshake::client(test_peer_id());
        let json = String::from_utf8(hs.to_json()).unwrap();

        assert!(json.contains("\"type\":\"handshake\""));
        assert!(json.contains("\"role\":\"client\""));
        // Client has no address, should be omitted
        assert!(!json.contains("\"address\""));
    }

    #[test]
    fn test_invalid_json() {
        assert!(Handshake::from_json(b"not json").is_none());
        assert!(Handshake::from_json(b"{}").is_none());
        assert!(Handshake::from_json(b"").is_none());
    }

    #[test]
    fn test_wrong_type() {
        let json = br#"{"type":"other","version":1,"peerId":"a1b2c3d4e5f67890","role":"server"}"#;
        assert!(Handshake::from_json(json).is_none());
    }

    // ==================== Version compatibility ====================

    #[test]
    fn test_same_version_compatible() {
        let hs = Handshake::client(test_peer_id());
        assert!(hs.is_compatible());
        assert!(!hs.should_warn_version());
    }

    #[test]
    fn test_different_version_compatible_but_warns() {
        let json = r#"{"type":"handshake","version":99,"peerId":"a1b2c3d4e5f67890","role":"client"}"#;
        let hs = Handshake::from_json(json.as_bytes()).unwrap();

        // Still compatible (graceful degradation)
        assert!(hs.is_compatible());
        // But should warn
        assert!(hs.should_warn_version());
    }

    // ==================== Role checks ====================

    #[test]
    fn test_is_server() {
        let server = Handshake::server(test_peer_id(), "ws://a:8080".into());
        assert!(server.is_server());
        assert!(!server.is_client_only());

        let client = Handshake::client(test_peer_id());
        assert!(!client.is_server());
        assert!(client.is_client_only());
    }

    #[test]
    fn test_server_without_address_is_client_only() {
        // Edge case: role is server but no address
        let hs = Handshake::new(test_peer_id(), HandshakeRole::Server, None);
        assert!(!hs.is_server()); // Can't be server without address
        assert!(hs.is_client_only());
    }

    // ==================== Role enum ====================

    #[test]
    fn test_role_enum() {
        assert!(HandshakeRole::Server.is_server());
        assert!(!HandshakeRole::Server.is_client());
        assert!(!HandshakeRole::Client.is_server());
        assert!(HandshakeRole::Client.is_client());
    }

    // ==================== Equality ====================

    #[test]
    fn test_equality() {
        let hs1 = Handshake::server(test_peer_id(), "ws://a:8080".into());
        let hs2 = Handshake::server(test_peer_id(), "ws://a:8080".into());
        let hs3 = Handshake::server(test_peer_id_2(), "ws://a:8080".into());
        let hs4 = Handshake::client(test_peer_id());

        assert_eq!(hs1, hs2);
        assert_ne!(hs1, hs3);
        assert_ne!(hs1, hs4);
    }
}
