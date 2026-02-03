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
    #[error("Unknown connection ID: {0}")]
    UnknownConnection(String),
}

/// Connection state for a peer.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum ConnectionState {
    /// WebSocket open, awaiting handshake
    Connecting,
    /// Handshake complete, fully connected
    Connected,
    /// Connection closed
    Disconnected,
}

/// Reason for disconnection.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub enum DisconnectReason {
    /// disconnect() called by user
    UserRequested,
    /// WebSocket error
    NetworkError,
    /// Remote peer closed the connection
    RemoteClosed,
    /// Invalid handshake or protocol violation
    ProtocolError,
    /// Duplicate connection detected (simultaneous connect)
    DuplicateConnection,
}

/// Result of checking for duplicate connections.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DuplicateCheckResult {
    /// No duplicate - proceed with connection
    NoDuplicate,
    /// Duplicate detected - close this connection (we have lower peer_id, keep outgoing)
    CloseThis,
    /// Duplicate detected - close the other connection (we have higher peer_id, keep incoming).
    /// The caller should look up the existing connection_id from its peer-to-connection mapping.
    CloseOther,
}

/// Check if we should close a connection due to simultaneous connect.
///
/// When two peers connect to each other simultaneously, we need to close one:
/// - Lower peer_id keeps their outgoing connection
/// - Higher peer_id keeps the incoming connection
///
/// This ensures both sides agree on which connection to keep.
pub fn check_duplicate_connection(
    our_peer_id: &str,
    their_peer_id: &str,
    new_direction: ConnectionDirection,
    existing_direction: Option<ConnectionDirection>,
) -> DuplicateCheckResult {
    // No existing connection = no duplicate
    let existing_dir = match existing_direction {
        Some(dir) => dir,
        None => return DuplicateCheckResult::NoDuplicate,
    };

    // Same direction = not a simultaneous connect, handle as reconnect
    if new_direction == existing_dir {
        return DuplicateCheckResult::NoDuplicate;
    }

    // Different directions = simultaneous connect
    // Lower peer_id wins with outgoing, higher peer_id wins with incoming
    if our_peer_id < their_peer_id {
        // We have lower ID - we keep outgoing
        if new_direction == ConnectionDirection::Incoming {
            // This is the incoming we should reject
            DuplicateCheckResult::CloseThis
        } else {
            // This is our outgoing - close the incoming
            DuplicateCheckResult::CloseOther
        }
    } else {
        // We have higher ID - we keep incoming
        if new_direction == ConnectionDirection::Outgoing {
            // This is our outgoing we should close
            DuplicateCheckResult::CloseThis
        } else {
            // This is incoming - close our outgoing
            DuplicateCheckResult::CloseOther
        }
    }
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
    /// Connection state
    pub state: ConnectionState,
    /// Reason for disconnection (if disconnected)
    pub disconnect_reason: Option<DisconnectReason>,
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
        /// Maps connection IDs to peer IDs (for pre-handshake → post-handshake resolution)
        connections: RwLock<HashMap<String, String>>,
    }

    impl Default for PeerRegistry {
        fn default() -> Self {
            Self {
                peers: RwLock::new(HashMap::new()),
                connections: RwLock::new(HashMap::new()),
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
                if peer.state == ConnectionState::Connected {
                    // Duplicate connect - idempotent, just update activity
                    peer.last_seen = timestamp;
                    peer.address = address;
                    peer.direction = direction;
                } else {
                    // Reconnection
                    peer.state = ConnectionState::Connected;
                    peer.disconnect_reason = None;
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
                    state: ConnectionState::Connected,
                    disconnect_reason: None,
                    first_seen: timestamp,
                    last_seen: timestamp,
                    connection_count: 1,
                };
                peers.insert(id, peer.clone());
                peer
            };

            Ok(peer)
        }

        /// Mark peer as disconnected (keeps in registry, sets state to Disconnected).
        ///
        /// Can disconnect by either peer ID or connection ID (for pre-handshake disconnects).
        pub fn peer_disconnected(
            &self,
            id: &str,
            reason: DisconnectReason,
            timestamp: f64,
        ) -> bool {
            let mut peers = self.peers.write().unwrap_or_else(|e| e.into_inner());
            let connections = self.connections.read().unwrap_or_else(|e| e.into_inner());

            // Try to resolve connection ID to peer ID
            let peer_id = connections.get(id).map(|s| s.as_str()).unwrap_or(id);

            if let Some(peer) = peers.get_mut(peer_id) {
                peer.state = ConnectionState::Disconnected;
                peer.disconnect_reason = Some(reason);
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
                .filter(|p| p.state == ConnectionState::Connected)
                .cloned()
                .collect()
        }

        /// Check if peer is currently connected.
        pub fn is_connected(&self, id: &str) -> bool {
            self.peers
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .get(id)
                .map(|p| p.state == ConnectionState::Connected)
                .unwrap_or(false)
        }

        /// Called when WebSocket opens (before handshake).
        /// Creates peer in Connecting state, indexed by connection ID.
        pub fn peer_connecting(
            &self,
            connection_id: String,
            address: String,
            direction: ConnectionDirection,
            timestamp: f64,
        ) -> ConnectedPeer {
            let mut peers = self.peers.write().unwrap_or_else(|e| e.into_inner());
            let mut connections = self.connections.write().unwrap_or_else(|e| e.into_inner());

            // Map connection_id to itself initially (before handshake)
            connections.insert(connection_id.clone(), connection_id.clone());

            let peer = ConnectedPeer {
                id: connection_id.clone(),
                address,
                direction,
                state: ConnectionState::Connecting,
                disconnect_reason: None,
                first_seen: timestamp,
                last_seen: timestamp,
                connection_count: 1,
            };
            peers.insert(connection_id, peer.clone());
            peer
        }

        /// Called when handshake completes. Maps connection_id to real peer_id.
        /// Returns error if connection_id unknown.
        pub fn peer_handshake_complete(
            &self,
            connection_id: &str,
            peer_id: String,
            timestamp: f64,
        ) -> Result<ConnectedPeer, super::PeerError> {
            let mut peers = self.peers.write().unwrap_or_else(|e| e.into_inner());
            let mut connections = self.connections.write().unwrap_or_else(|e| e.into_inner());

            // Get the existing connecting peer
            let mut peer = peers
                .remove(connection_id)
                .ok_or_else(|| super::PeerError::UnknownConnection(connection_id.to_string()))?;

            // Update to connected state with real peer ID
            peer.id = peer_id.clone();
            peer.state = ConnectionState::Connected;
            peer.last_seen = timestamp;

            // Update connection mapping
            connections.insert(connection_id.to_string(), peer_id.clone());

            // Re-insert under real peer ID
            peers.insert(peer_id, peer.clone());

            Ok(peer)
        }

        /// Get peer by connection ID (for pre-handshake lookups).
        pub fn get_peer_by_connection_id(&self, connection_id: &str) -> Option<ConnectedPeer> {
            let connections = self.connections.read().unwrap_or_else(|e| e.into_inner());
            let peer_id = connections.get(connection_id)?;
            self.peers
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .get(peer_id)
                .cloned()
        }

        /// Resolve connection ID to peer ID (returns connection_id if no mapping).
        pub fn resolve_peer_id(&self, connection_id: &str) -> String {
            self.connections
                .read()
                .unwrap_or_else(|e| e.into_inner())
                .get(connection_id)
                .cloned()
                .unwrap_or_else(|| connection_id.to_string())
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
        /// Maps connection IDs to peer IDs (for pre-handshake → post-handshake resolution)
        connections: RefCell<HashMap<String, String>>,
    }

    impl Default for PeerRegistry {
        fn default() -> Self {
            Self {
                peers: RefCell::new(HashMap::new()),
                connections: RefCell::new(HashMap::new()),
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
                if peer.state == ConnectionState::Connected {
                    // Duplicate connect - idempotent, just update activity
                    peer.last_seen = timestamp;
                    peer.address = address;
                    peer.direction = direction;
                } else {
                    // Reconnection
                    peer.state = ConnectionState::Connected;
                    peer.disconnect_reason = None;
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
                    state: ConnectionState::Connected,
                    disconnect_reason: None,
                    first_seen: timestamp,
                    last_seen: timestamp,
                    connection_count: 1,
                };
                peers.insert(id, peer.clone());
                peer
            };

            Ok(peer)
        }

        /// Mark peer as disconnected (keeps in registry, sets state to Disconnected).
        ///
        /// Can disconnect by either peer ID or connection ID (for pre-handshake disconnects).
        pub fn peer_disconnected(
            &self,
            id: &str,
            reason: DisconnectReason,
            timestamp: f64,
        ) -> bool {
            let mut peers = self.peers.borrow_mut();
            let connections = self.connections.borrow();

            // Try to resolve connection ID to peer ID
            let peer_id = connections.get(id).map(|s| s.as_str()).unwrap_or(id);

            if let Some(peer) = peers.get_mut(peer_id) {
                peer.state = ConnectionState::Disconnected;
                peer.disconnect_reason = Some(reason);
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
                .filter(|p| p.state == ConnectionState::Connected)
                .cloned()
                .collect()
        }

        /// Check if peer is currently connected.
        pub fn is_connected(&self, id: &str) -> bool {
            self.peers
                .borrow()
                .get(id)
                .map(|p| p.state == ConnectionState::Connected)
                .unwrap_or(false)
        }

        /// Called when WebSocket opens (before handshake).
        /// Creates peer in Connecting state, indexed by connection ID.
        pub fn peer_connecting(
            &self,
            connection_id: String,
            address: String,
            direction: ConnectionDirection,
            timestamp: f64,
        ) -> ConnectedPeer {
            let mut peers = self.peers.borrow_mut();
            let mut connections = self.connections.borrow_mut();

            // Map connection_id to itself initially (before handshake)
            connections.insert(connection_id.clone(), connection_id.clone());

            let peer = ConnectedPeer {
                id: connection_id.clone(),
                address,
                direction,
                state: ConnectionState::Connecting,
                disconnect_reason: None,
                first_seen: timestamp,
                last_seen: timestamp,
                connection_count: 1,
            };
            peers.insert(connection_id, peer.clone());
            peer
        }

        /// Called when handshake completes. Maps connection_id to real peer_id.
        /// Returns error if connection_id unknown.
        pub fn peer_handshake_complete(
            &self,
            connection_id: &str,
            peer_id: String,
            timestamp: f64,
        ) -> Result<ConnectedPeer, super::PeerError> {
            let mut peers = self.peers.borrow_mut();
            let mut connections = self.connections.borrow_mut();

            // Get the existing connecting peer
            let mut peer = peers
                .remove(connection_id)
                .ok_or_else(|| super::PeerError::UnknownConnection(connection_id.to_string()))?;

            // Update to connected state with real peer ID
            peer.id = peer_id.clone();
            peer.state = ConnectionState::Connected;
            peer.last_seen = timestamp;

            // Update connection mapping
            connections.insert(connection_id.to_string(), peer_id.clone());

            // Re-insert under real peer ID
            peers.insert(peer_id, peer.clone());

            Ok(peer)
        }

        /// Get peer by connection ID (for pre-handshake lookups).
        pub fn get_peer_by_connection_id(&self, connection_id: &str) -> Option<ConnectedPeer> {
            let connections = self.connections.borrow();
            let peer_id = connections.get(connection_id)?;
            self.peers.borrow().get(peer_id).cloned()
        }

        /// Resolve connection ID to peer ID (returns connection_id if no mapping).
        pub fn resolve_peer_id(&self, connection_id: &str) -> String {
            self.connections
                .borrow()
                .get(connection_id)
                .cloned()
                .unwrap_or_else(|| connection_id.to_string())
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
        assert_eq!(peer.state, ConnectionState::Connected);
        assert_eq!(peer.disconnect_reason, None);
        assert_eq!(peer.first_seen, 1000.0);
        assert_eq!(peer.last_seen, 1000.0);
        assert_eq!(peer.connection_count, 1);
    }

    #[test]
    fn test_disconnect_sets_state_to_disconnected() {
        let registry = PeerRegistry::new();
        registry
            .peer_connected(
                "peer1".into(),
                "192.168.1.1:8080".into(),
                ConnectionDirection::Incoming,
                1000.0,
            )
            .unwrap();

        let result = registry.peer_disconnected("peer1", DisconnectReason::NetworkError, 2000.0);
        assert!(result);

        let peer = registry.get_peer("peer1").unwrap();
        assert_eq!(peer.state, ConnectionState::Disconnected);
        assert_eq!(peer.disconnect_reason, Some(DisconnectReason::NetworkError));
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
        registry.peer_disconnected("peer1", DisconnectReason::UserRequested, 2000.0);

        // Reconnect with different address
        let peer = registry
            .peer_connected(
                "peer1".into(),
                "addr2".into(),
                ConnectionDirection::Outgoing,
                3000.0,
            )
            .unwrap();

        assert_eq!(peer.state, ConnectionState::Connected);
        assert_eq!(peer.disconnect_reason, None);
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

        assert_eq!(peer.state, ConnectionState::Connected);
        assert_eq!(peer.connection_count, 1); // NOT incremented
        assert_eq!(peer.last_seen, 2000.0); // Updated
        assert_eq!(peer.address, "addr2"); // Updated
        assert_eq!(peer.direction, ConnectionDirection::Outgoing); // Updated
    }

    #[test]
    fn test_disconnect_unknown_peer_returns_false() {
        let registry = PeerRegistry::new();
        let result = registry.peer_disconnected("unknown", DisconnectReason::NetworkError, 1000.0);
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
        registry.peer_disconnected("peer1", DisconnectReason::UserRequested, 2000.0);
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
        registry.peer_disconnected("peer1", DisconnectReason::UserRequested, 2000.0);

        // Disconnect again
        let result = registry.peer_disconnected("peer1", DisconnectReason::UserRequested, 3000.0);
        assert!(result); // Returns true (peer exists)

        let peer = registry.get_peer("peer1").unwrap();
        assert_eq!(peer.state, ConnectionState::Disconnected);
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
        registry.peer_disconnected("peer1", DisconnectReason::UserRequested, 2000.0);

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
        registry.peer_disconnected("peer1", DisconnectReason::UserRequested, 2000.0);

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
        registry.peer_disconnected("peer1", DisconnectReason::UserRequested, 2000.0);

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
        registry.peer_disconnected("peer1", DisconnectReason::UserRequested, 2000.0);

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

    // ========== Connection ID mapping tests ==========

    #[test]
    fn test_peer_connecting_creates_connecting_state() {
        let registry = PeerRegistry::new();
        let peer = registry.peer_connecting(
            "conn-1".into(),
            "192.168.1.1:8765".into(),
            ConnectionDirection::Outgoing,
            1000.0,
        );

        assert_eq!(peer.id, "conn-1");
        assert_eq!(peer.state, ConnectionState::Connecting);
        assert_eq!(peer.disconnect_reason, None);
        assert_eq!(peer.connection_count, 1);
    }

    #[test]
    fn test_peer_handshake_complete_transitions_to_connected() {
        let registry = PeerRegistry::new();

        // Start connecting
        registry.peer_connecting(
            "conn-1".into(),
            "192.168.1.1:8765".into(),
            ConnectionDirection::Outgoing,
            1000.0,
        );

        // Complete handshake
        let peer = registry
            .peer_handshake_complete("conn-1", "real-peer-id".into(), 2000.0)
            .unwrap();

        assert_eq!(peer.id, "real-peer-id");
        assert_eq!(peer.state, ConnectionState::Connected);
        assert_eq!(peer.last_seen, 2000.0);
    }

    #[test]
    fn test_connection_id_to_peer_id_mapping() {
        let registry = PeerRegistry::new();

        // Before handshake, peer indexed by connection ID
        registry.peer_connecting(
            "conn-1".into(),
            "addr".into(),
            ConnectionDirection::Outgoing,
            1000.0,
        );
        assert!(registry.get_peer_by_connection_id("conn-1").is_some());
        assert!(registry.get_peer("conn-1").is_some()); // Accessible by connection ID
        assert!(registry.get_peer("peer-abc").is_none()); // Not by real ID yet

        // After handshake, accessible by both
        registry
            .peer_handshake_complete("conn-1", "peer-abc".into(), 2000.0)
            .unwrap();
        assert!(registry.get_peer("peer-abc").is_some());
        assert!(registry.get_peer_by_connection_id("conn-1").is_some());
        assert_eq!(registry.resolve_peer_id("conn-1"), "peer-abc");
    }

    #[test]
    fn test_handshake_unknown_connection_fails() {
        let registry = PeerRegistry::new();
        let result = registry.peer_handshake_complete("unknown", "peer-1".into(), 1000.0);
        assert!(matches!(
            result.unwrap_err(),
            PeerError::UnknownConnection(_)
        ));
    }

    #[test]
    fn test_resolve_peer_id_returns_input_if_unknown() {
        let registry = PeerRegistry::new();
        assert_eq!(registry.resolve_peer_id("unknown-conn"), "unknown-conn");
    }

    #[test]
    fn test_connection_state_transitions() {
        let registry = PeerRegistry::new();

        // Connecting state
        let peer = registry.peer_connecting(
            "temp-1".into(),
            "addr".into(),
            ConnectionDirection::Outgoing,
            1000.0,
        );
        assert_eq!(peer.state, ConnectionState::Connecting);

        // Connected after handshake
        let peer = registry
            .peer_handshake_complete("temp-1", "real-id".into(), 2000.0)
            .unwrap();
        assert_eq!(peer.state, ConnectionState::Connected);
        assert_eq!(peer.id, "real-id");

        // Disconnected
        registry.peer_disconnected("real-id", DisconnectReason::NetworkError, 3000.0);
        let peer = registry.get_peer("real-id").unwrap();
        assert_eq!(peer.state, ConnectionState::Disconnected);
        assert_eq!(peer.disconnect_reason, Some(DisconnectReason::NetworkError));
    }

    #[test]
    fn test_disconnect_by_connection_id() {
        let registry = PeerRegistry::new();

        // Start connecting
        registry.peer_connecting(
            "conn-1".into(),
            "addr".into(),
            ConnectionDirection::Outgoing,
            1000.0,
        );

        // Disconnect before handshake using connection ID
        let result =
            registry.peer_disconnected("conn-1", DisconnectReason::NetworkError, 2000.0);
        assert!(result);

        let peer = registry.get_peer("conn-1").unwrap();
        assert_eq!(peer.state, ConnectionState::Disconnected);
        assert_eq!(peer.disconnect_reason, Some(DisconnectReason::NetworkError));
    }

    #[test]
    fn test_disconnect_records_reason() {
        let registry = PeerRegistry::new();
        registry.peer_connecting(
            "conn-1".into(),
            "addr".into(),
            ConnectionDirection::Outgoing,
            1000.0,
        );
        registry
            .peer_handshake_complete("conn-1", "peer-1".into(), 2000.0)
            .unwrap();

        registry.peer_disconnected("peer-1", DisconnectReason::UserRequested, 3000.0);

        let peer = registry.get_peer("peer-1").unwrap();
        assert_eq!(peer.disconnect_reason, Some(DisconnectReason::UserRequested));
    }

    // ========== Connection deduplication tests ==========

    #[test]
    fn test_dedup_no_existing_connection() {
        let result = check_duplicate_connection(
            "aaaa",
            "bbbb",
            ConnectionDirection::Incoming,
            None, // No existing connection
        );
        assert_eq!(result, DuplicateCheckResult::NoDuplicate);
    }

    #[test]
    fn test_dedup_same_direction_not_duplicate() {
        // Both incoming = not simultaneous connect (it's a reconnect scenario)
        let result = check_duplicate_connection(
            "aaaa",
            "bbbb",
            ConnectionDirection::Incoming,
            Some(ConnectionDirection::Incoming),
        );
        assert_eq!(result, DuplicateCheckResult::NoDuplicate);

        // Both outgoing = also not simultaneous connect
        let result = check_duplicate_connection(
            "aaaa",
            "bbbb",
            ConnectionDirection::Outgoing,
            Some(ConnectionDirection::Outgoing),
        );
        assert_eq!(result, DuplicateCheckResult::NoDuplicate);
    }

    #[test]
    fn test_dedup_lower_peer_keeps_outgoing() {
        // We are "aaaa" (lower), they are "bbbb" (higher)
        // We should keep our outgoing connection

        // New incoming from bbbb, we already have outgoing to bbbb
        let result = check_duplicate_connection(
            "aaaa",
            "bbbb",
            ConnectionDirection::Incoming, // New connection
            Some(ConnectionDirection::Outgoing), // Existing connection
        );
        // We should close the new incoming
        assert_eq!(result, DuplicateCheckResult::CloseThis);
    }

    #[test]
    fn test_dedup_lower_peer_with_outgoing_closes_incoming() {
        // We are "aaaa" (lower), new connection is outgoing
        // We already have incoming from bbbb
        let result = check_duplicate_connection(
            "aaaa",
            "bbbb",
            ConnectionDirection::Outgoing, // New connection
            Some(ConnectionDirection::Incoming), // Existing connection
        );
        // We should close the other (incoming)
        assert_eq!(result, DuplicateCheckResult::CloseOther);
    }

    #[test]
    fn test_dedup_higher_peer_keeps_incoming() {
        // We are "bbbb" (higher), they are "aaaa" (lower)
        // We should keep the incoming connection

        // New outgoing to aaaa, we already have incoming from aaaa
        let result = check_duplicate_connection(
            "bbbb",
            "aaaa",
            ConnectionDirection::Outgoing, // New connection
            Some(ConnectionDirection::Incoming), // Existing connection
        );
        // We should close our outgoing
        assert_eq!(result, DuplicateCheckResult::CloseThis);
    }

    #[test]
    fn test_dedup_higher_peer_with_incoming_closes_outgoing() {
        // We are "bbbb" (higher), new connection is incoming
        // We already have outgoing to aaaa
        let result = check_duplicate_connection(
            "bbbb",
            "aaaa",
            ConnectionDirection::Incoming, // New connection
            Some(ConnectionDirection::Outgoing), // Existing connection
        );
        // We should close the other (outgoing)
        assert_eq!(result, DuplicateCheckResult::CloseOther);
    }

    #[test]
    fn test_dedup_symmetric_agreement() {
        // Both sides should agree on which connection to keep
        // Simulate simultaneous connect: A(aaaa) and B(bbbb) connect to each other

        // From A's perspective: B sent incoming, A has outgoing to B
        let a_perspective = check_duplicate_connection(
            "aaaa", // A is lower
            "bbbb",
            ConnectionDirection::Incoming, // B's connection to A
            Some(ConnectionDirection::Outgoing), // A's connection to B
        );
        // A keeps outgoing, closes incoming
        assert_eq!(a_perspective, DuplicateCheckResult::CloseThis);

        // From B's perspective: A sent incoming, B has outgoing to A
        let b_perspective = check_duplicate_connection(
            "bbbb", // B is higher
            "aaaa",
            ConnectionDirection::Incoming, // A's connection to B
            Some(ConnectionDirection::Outgoing), // B's connection to A
        );
        // B keeps incoming (from A), closes their outgoing
        assert_eq!(b_perspective, DuplicateCheckResult::CloseOther);

        // Result: A's outgoing to B survives = B's incoming from A survives
        // Both sides keep the same connection!
    }

    #[test]
    fn test_disconnect_reason_duplicate() {
        let registry = PeerRegistry::new();
        registry
            .peer_connected(
                "peer-1".into(),
                "addr".into(),
                ConnectionDirection::Outgoing,
                1000.0,
            )
            .unwrap();

        registry.peer_disconnected("peer-1", DisconnectReason::DuplicateConnection, 2000.0);

        let peer = registry.get_peer("peer-1").unwrap();
        assert_eq!(
            peer.disconnect_reason,
            Some(DisconnectReason::DuplicateConnection)
        );
    }
}
