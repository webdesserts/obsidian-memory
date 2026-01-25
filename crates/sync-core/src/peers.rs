//! Peer registry for tracking connected peers.
//!
//! Provides `PeerRegistry` for tracking peer connection state and `ConnectedPeer` for peer metadata.
//! Platform-specific implementations handle thread safety:
//! - Native: `Arc<PeerRegistry>` with `RwLock` for multi-threaded Tokio runtime
//! - WASM: `Rc<PeerRegistry>` with `RefCell` for single-threaded browser environment

use serde::Serialize;
use std::collections::HashMap;
use thiserror::Error;

/// Errors that can occur during peer operations.
#[derive(Debug, Error)]
pub enum PeerError {
    #[error("Peer ID cannot be empty")]
    EmptyId,
}

/// Connection direction from our perspective.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ConnectionDirection {
    Incoming,
    Outgoing,
}

/// Tracked state for a peer in the registry.
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ConnectedPeer {
    /// Peer's unique identifier (from handshake)
    pub id: String,
    /// Connection address (IP:port or URL)
    pub address: String,
    /// Connection direction
    pub direction: ConnectionDirection,
    /// Currently connected?
    pub connected: bool,
    /// When first seen this session (ms since epoch)
    pub first_seen: f64,
    /// When last activity observed (ms since epoch)
    pub last_seen: f64,
    /// Times this peer has connected this session
    pub connection_count: u32,
}

// ============================================================================
// Native (multi-threaded) implementation
// ============================================================================

#[cfg(not(target_arch = "wasm32"))]
mod platform {
    use super::*;
    use std::sync::RwLock;

    /// Registry for tracking connected peers.
    ///
    /// Thread-safe for use in multi-threaded Tokio runtime.
    /// Wrap in `Arc` for shared ownership.
    pub struct PeerRegistry {
        peers: RwLock<HashMap<String, ConnectedPeer>>,
    }

    impl Default for PeerRegistry {
        fn default() -> Self {
            Self {
                peers: RwLock::new(HashMap::new()),
            }
        }
    }

    impl PeerRegistry {
        pub fn new() -> Self {
            Self::default()
        }

        /// Record a connection. If peer exists, increments connection_count and updates timestamps.
        ///
        /// Returns an error if the peer ID is empty.
        pub fn peer_connected(
            &self,
            id: String,
            address: String,
            direction: ConnectionDirection,
            timestamp: f64,
        ) -> Result<ConnectedPeer, super::PeerError> {
            if id.is_empty() {
                return Err(super::PeerError::EmptyId);
            }

            let mut peers = self.peers.write().unwrap_or_else(|e| e.into_inner());

            let peer = if let Some(peer) = peers.get_mut(&id) {
                // Existing peer
                if peer.connected {
                    // Duplicate connect - idempotent, just update activity
                    peer.last_seen = timestamp;
                    peer.address = address;
                    peer.direction = direction;
                } else {
                    // Reconnection
                    peer.connected = true;
                    peer.connection_count += 1;
                    peer.last_seen = timestamp;
                    peer.address = address;
                    peer.direction = direction;
                }
                peer.clone()
            } else {
                // New peer
                let peer = ConnectedPeer {
                    id: id.clone(),
                    address,
                    direction,
                    connected: true,
                    first_seen: timestamp,
                    last_seen: timestamp,
                    connection_count: 1,
                };
                peers.insert(id, peer.clone());
                peer
            };

            Ok(peer)
        }

        /// Mark peer as disconnected (keeps in registry, sets connected=false).
        pub fn peer_disconnected(&self, id: &str, timestamp: f64) -> bool {
            let mut peers = self.peers.write().unwrap_or_else(|e| e.into_inner());

            if let Some(peer) = peers.get_mut(id) {
                peer.connected = false;
                peer.last_seen = timestamp;
                true
            } else {
                false
            }
        }

        /// Update last_seen timestamp.
        pub fn touch(&self, id: &str, timestamp: f64) {
            let mut peers = self.peers.write().unwrap_or_else(|e| e.into_inner());
            if let Some(peer) = peers.get_mut(id) {
                peer.last_seen = timestamp;
            }
        }

        /// All peers seen this session.
        pub fn get_known_peers(&self) -> Vec<ConnectedPeer> {
            self.peers
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .values()
                .cloned()
                .collect()
        }

        /// Specific peer info.
        pub fn get_peer(&self, id: &str) -> Option<ConnectedPeer> {
            self.peers
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .get(id)
                .cloned()
        }

        /// Currently connected peers only.
        pub fn get_connected_peers(&self) -> Vec<ConnectedPeer> {
            self.peers
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .values()
                .filter(|p| p.connected)
                .cloned()
                .collect()
        }

        /// Check if peer is currently connected.
        pub fn is_connected(&self, id: &str) -> bool {
            self.peers
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .get(id)
                .map(|p| p.connected)
                .unwrap_or(false)
        }
    }
}

// ============================================================================
// WASM (single-threaded) implementation
// ============================================================================

#[cfg(target_arch = "wasm32")]
mod platform {
    use super::*;
    use std::cell::RefCell;

    /// Registry for tracking connected peers.
    ///
    /// Single-threaded for WASM browser environment.
    /// Wrap in `Rc` for shared ownership.
    pub struct PeerRegistry {
        peers: RefCell<HashMap<String, ConnectedPeer>>,
    }

    impl Default for PeerRegistry {
        fn default() -> Self {
            Self {
                peers: RefCell::new(HashMap::new()),
            }
        }
    }

    impl PeerRegistry {
        pub fn new() -> Self {
            Self::default()
        }

        /// Record a connection. If peer exists, increments connection_count and updates timestamps.
        ///
        /// Returns an error if the peer ID is empty.
        pub fn peer_connected(
            &self,
            id: String,
            address: String,
            direction: ConnectionDirection,
            timestamp: f64,
        ) -> Result<ConnectedPeer, super::PeerError> {
            if id.is_empty() {
                return Err(super::PeerError::EmptyId);
            }

            let mut peers = self.peers.borrow_mut();

            let peer = if let Some(peer) = peers.get_mut(&id) {
                // Existing peer
                if peer.connected {
                    // Duplicate connect - idempotent, just update activity
                    peer.last_seen = timestamp;
                    peer.address = address;
                    peer.direction = direction;
                } else {
                    // Reconnection
                    peer.connected = true;
                    peer.connection_count += 1;
                    peer.last_seen = timestamp;
                    peer.address = address;
                    peer.direction = direction;
                }
                peer.clone()
            } else {
                // New peer
                let peer = ConnectedPeer {
                    id: id.clone(),
                    address,
                    direction,
                    connected: true,
                    first_seen: timestamp,
                    last_seen: timestamp,
                    connection_count: 1,
                };
                peers.insert(id, peer.clone());
                peer
            };

            Ok(peer)
        }

        /// Mark peer as disconnected (keeps in registry, sets connected=false).
        pub fn peer_disconnected(&self, id: &str, timestamp: f64) -> bool {
            let mut peers = self.peers.borrow_mut();

            if let Some(peer) = peers.get_mut(id) {
                peer.connected = false;
                peer.last_seen = timestamp;
                true
            } else {
                false
            }
        }

        /// Update last_seen timestamp.
        pub fn touch(&self, id: &str, timestamp: f64) {
            let mut peers = self.peers.borrow_mut();
            if let Some(peer) = peers.get_mut(id) {
                peer.last_seen = timestamp;
            }
        }

        /// All peers seen this session.
        pub fn get_known_peers(&self) -> Vec<ConnectedPeer> {
            self.peers.borrow().values().cloned().collect()
        }

        /// Specific peer info.
        pub fn get_peer(&self, id: &str) -> Option<ConnectedPeer> {
            self.peers.borrow().get(id).cloned()
        }

        /// Currently connected peers only.
        pub fn get_connected_peers(&self) -> Vec<ConnectedPeer> {
            self.peers
                .borrow()
                .values()
                .filter(|p| p.connected)
                .cloned()
                .collect()
        }

        /// Check if peer is currently connected.
        pub fn is_connected(&self, id: &str) -> bool {
            self.peers
                .borrow()
                .get(id)
                .map(|p| p.connected)
                .unwrap_or(false)
        }
    }
}

pub use platform::*;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_first_connection_creates_entry() {
        let registry = PeerRegistry::new();
        let peer = registry
            .peer_connected(
                "peer1".into(),
                "192.168.1.1:8080".into(),
                ConnectionDirection::Incoming,
                1000.0,
            )
            .unwrap();

        assert_eq!(peer.id, "peer1");
        assert_eq!(peer.address, "192.168.1.1:8080");
        assert_eq!(peer.direction, ConnectionDirection::Incoming);
        assert!(peer.connected);
        assert_eq!(peer.first_seen, 1000.0);
        assert_eq!(peer.last_seen, 1000.0);
        assert_eq!(peer.connection_count, 1);
    }

    #[test]
    fn test_disconnect_sets_connected_false() {
        let registry = PeerRegistry::new();
        registry
            .peer_connected(
                "peer1".into(),
                "192.168.1.1:8080".into(),
                ConnectionDirection::Incoming,
                1000.0,
            )
            .unwrap();

        let result = registry.peer_disconnected("peer1", 2000.0);
        assert!(result);

        let peer = registry.get_peer("peer1").unwrap();
        assert!(!peer.connected);
        assert_eq!(peer.last_seen, 2000.0);
        assert_eq!(peer.first_seen, 1000.0); // Preserved
        assert_eq!(peer.connection_count, 1); // Unchanged
    }

    #[test]
    fn test_reconnect_increments_count_preserves_first_seen() {
        let registry = PeerRegistry::new();

        // First connect
        registry
            .peer_connected(
                "peer1".into(),
                "addr1".into(),
                ConnectionDirection::Incoming,
                1000.0,
            )
            .unwrap();

        // Disconnect
        registry.peer_disconnected("peer1", 2000.0);

        // Reconnect with different address
        let peer = registry
            .peer_connected(
                "peer1".into(),
                "addr2".into(),
                ConnectionDirection::Outgoing,
                3000.0,
            )
            .unwrap();

        assert!(peer.connected);
        assert_eq!(peer.connection_count, 2);
        assert_eq!(peer.first_seen, 1000.0); // Preserved
        assert_eq!(peer.last_seen, 3000.0);
        assert_eq!(peer.address, "addr2"); // Updated
        assert_eq!(peer.direction, ConnectionDirection::Outgoing); // Updated
    }

    #[test]
    fn test_duplicate_connect_is_idempotent() {
        let registry = PeerRegistry::new();

        // First connect
        registry
            .peer_connected(
                "peer1".into(),
                "addr1".into(),
                ConnectionDirection::Incoming,
                1000.0,
            )
            .unwrap();

        // Duplicate connect (same peer, already connected)
        let peer = registry
            .peer_connected(
                "peer1".into(),
                "addr2".into(),
                ConnectionDirection::Outgoing,
                2000.0,
            )
            .unwrap();

        assert!(peer.connected);
        assert_eq!(peer.connection_count, 1); // NOT incremented
        assert_eq!(peer.last_seen, 2000.0); // Updated
        assert_eq!(peer.address, "addr2"); // Updated
        assert_eq!(peer.direction, ConnectionDirection::Outgoing); // Updated
    }

    #[test]
    fn test_disconnect_unknown_peer_returns_false() {
        let registry = PeerRegistry::new();
        let result = registry.peer_disconnected("unknown", 1000.0);
        assert!(!result);

        // Should not create an entry
        assert!(registry.get_peer("unknown").is_none());
    }

    #[test]
    fn test_full_connect_disconnect_cycle() {
        let registry = PeerRegistry::new();

        // Connect
        registry
            .peer_connected(
                "peer1".into(),
                "addr".into(),
                ConnectionDirection::Incoming,
                1000.0,
            )
            .unwrap();
        assert!(registry.is_connected("peer1"));

        // Disconnect
        registry.peer_disconnected("peer1", 2000.0);
        assert!(!registry.is_connected("peer1"));

        // Reconnect
        registry
            .peer_connected(
                "peer1".into(),
                "addr".into(),
                ConnectionDirection::Incoming,
                3000.0,
            )
            .unwrap();
        assert!(registry.is_connected("peer1"));

        let peer = registry.get_peer("peer1").unwrap();
        assert_eq!(peer.connection_count, 2);
    }

    #[test]
    fn test_disconnect_already_disconnected_is_idempotent() {
        let registry = PeerRegistry::new();

        registry
            .peer_connected(
                "peer1".into(),
                "addr".into(),
                ConnectionDirection::Incoming,
                1000.0,
            )
            .unwrap();
        registry.peer_disconnected("peer1", 2000.0);

        // Disconnect again
        let result = registry.peer_disconnected("peer1", 3000.0);
        assert!(result); // Returns true (peer exists)

        let peer = registry.get_peer("peer1").unwrap();
        assert!(!peer.connected);
        assert_eq!(peer.last_seen, 3000.0); // Updated
    }

    #[test]
    fn test_get_connected_peers_excludes_disconnected() {
        let registry = PeerRegistry::new();

        registry
            .peer_connected(
                "peer1".into(),
                "addr1".into(),
                ConnectionDirection::Incoming,
                1000.0,
            )
            .unwrap();
        registry
            .peer_connected(
                "peer2".into(),
                "addr2".into(),
                ConnectionDirection::Outgoing,
                1000.0,
            )
            .unwrap();
        registry.peer_disconnected("peer1", 2000.0);

        let connected = registry.get_connected_peers();
        assert_eq!(connected.len(), 1);
        assert_eq!(connected[0].id, "peer2");
    }

    #[test]
    fn test_get_known_peers_includes_all() {
        let registry = PeerRegistry::new();

        registry
            .peer_connected(
                "peer1".into(),
                "addr1".into(),
                ConnectionDirection::Incoming,
                1000.0,
            )
            .unwrap();
        registry
            .peer_connected(
                "peer2".into(),
                "addr2".into(),
                ConnectionDirection::Outgoing,
                1000.0,
            )
            .unwrap();
        registry.peer_disconnected("peer1", 2000.0);

        let known = registry.get_known_peers();
        assert_eq!(known.len(), 2);
    }

    #[test]
    fn test_get_peer_returns_none_for_unknown() {
        let registry = PeerRegistry::new();
        assert!(registry.get_peer("unknown").is_none());
    }

    #[test]
    fn test_is_connected_returns_false_for_unknown() {
        let registry = PeerRegistry::new();
        assert!(!registry.is_connected("unknown"));
    }

    #[test]
    fn test_empty_registry() {
        let registry = PeerRegistry::new();
        assert!(registry.get_known_peers().is_empty());
        assert!(registry.get_connected_peers().is_empty());
    }

    #[test]
    fn test_touch_updates_last_seen() {
        let registry = PeerRegistry::new();

        registry
            .peer_connected(
                "peer1".into(),
                "addr".into(),
                ConnectionDirection::Incoming,
                1000.0,
            )
            .unwrap();

        registry.touch("peer1", 5000.0);

        let peer = registry.get_peer("peer1").unwrap();
        assert_eq!(peer.last_seen, 5000.0);
        assert_eq!(peer.first_seen, 1000.0); // Unchanged
    }

    #[test]
    fn test_touch_unknown_peer_is_silent() {
        let registry = PeerRegistry::new();
        registry.touch("unknown", 1000.0); // Should not panic
        assert!(registry.get_peer("unknown").is_none());
    }

    #[test]
    fn test_empty_id_returns_error() {
        let registry = PeerRegistry::new();
        let result = registry.peer_connected(
            "".into(),
            "addr".into(),
            ConnectionDirection::Incoming,
            1000.0,
        );

        assert!(result.is_err());
        assert!(matches!(result.unwrap_err(), PeerError::EmptyId));
        assert!(registry.get_known_peers().is_empty());
    }

    #[test]
    fn test_address_update_on_reconnect() {
        let registry = PeerRegistry::new();

        registry
            .peer_connected(
                "peer1".into(),
                "old-addr".into(),
                ConnectionDirection::Incoming,
                1000.0,
            )
            .unwrap();
        registry.peer_disconnected("peer1", 2000.0);

        let peer = registry
            .peer_connected(
                "peer1".into(),
                "new-addr".into(),
                ConnectionDirection::Incoming,
                3000.0,
            )
            .unwrap();

        assert_eq!(peer.address, "new-addr");
    }

    #[test]
    fn test_direction_change_on_reconnect() {
        let registry = PeerRegistry::new();

        registry
            .peer_connected(
                "peer1".into(),
                "addr".into(),
                ConnectionDirection::Incoming,
                1000.0,
            )
            .unwrap();
        registry.peer_disconnected("peer1", 2000.0);

        let peer = registry
            .peer_connected(
                "peer1".into(),
                "addr".into(),
                ConnectionDirection::Outgoing,
                3000.0,
            )
            .unwrap();

        assert_eq!(peer.direction, ConnectionDirection::Outgoing);
    }
}
