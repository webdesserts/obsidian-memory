//! Unified connection manager for P2P sync.
//!
//! Manages both incoming and outgoing WebSocket connections, providing:
//! - Unified interface for sending/broadcasting messages
//! - Integration with SWIM gossip protocol
//! - Priority queuing (SWIM messages sent before sync data)
//! - Connection deduplication
//! - Automatic reconnection for outgoing connections

use crate::connection::{ConnectionEvent, IncomingMessage, PeerConnection};
use crate::outgoing::{OutgoingConnection, OutgoingState, ReconnectConfig};
use anyhow::Result;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::{SystemTime, UNIX_EPOCH};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio_tungstenite::accept_async;
use tracing::{debug, error, info, warn};

use sync_core::peers::{
    check_duplicate_connection, ConnectionDirection, DisconnectReason, DuplicateCheckResult,
};

/// Event emitted by the connection manager.
#[derive(Debug)]
pub enum ManagerEvent {
    /// Received a message from a peer (could be SWIM or sync)
    Message(IncomingMessage),
    /// Handshake completed, peer identity known
    HandshakeComplete {
        connection_id: String,
        peer_id: String,
        direction: ConnectionDirection,
        address: Option<String>,
    },
    /// Connection closed
    ConnectionClosed {
        peer_id: String,
        reason: DisconnectReason,
    },
    /// New peer discovered via gossip (should auto-connect if server)
    PeerDiscovered { peer_id: String, address: String },
}

/// A unified connection (incoming or outgoing).
enum Connection {
    Incoming(PeerConnection),
    Outgoing(OutgoingConnection),
}

impl Connection {
    fn direction(&self) -> ConnectionDirection {
        match self {
            Connection::Incoming(_) => ConnectionDirection::Incoming,
            Connection::Outgoing(_) => ConnectionDirection::Outgoing,
        }
    }

    fn peer_id(&self) -> Option<&str> {
        match self {
            Connection::Incoming(c) => c.real_peer_id.as_deref(),
            Connection::Outgoing(c) => c.remote_peer_id.as_deref(),
        }
    }

    async fn send(&self, data: &[u8]) -> Result<()> {
        match self {
            Connection::Incoming(c) => c.send(data).await,
            Connection::Outgoing(c) => c.send(data).await,
        }
    }
}

/// Unified connection manager.
pub struct ConnectionManager {
    /// Our peer ID
    our_peer_id: String,
    /// Our advertised address (None if client-only)
    our_address: Option<String>,
    /// All connections indexed by connection ID (temp_id for incoming, address for outgoing)
    connections: HashMap<String, Connection>,
    /// Map from peer ID to connection ID (for routing by peer ID)
    peer_to_conn: HashMap<String, String>,
    /// Counter for generating temp IDs
    next_conn_id: u64,
    /// Channel sender for connection events
    event_tx: mpsc::UnboundedSender<ConnectionEvent>,
    /// Channel receiver for internal events
    event_rx: mpsc::UnboundedReceiver<ConnectionEvent>,
    /// Sender for manager events (to main loop)
    manager_tx: mpsc::UnboundedSender<ManagerEvent>,
    /// Reconnection configuration
    reconnect_config: ReconnectConfig,
}

impl ConnectionManager {
    /// Create a new connection manager.
    pub fn new(
        our_peer_id: String,
        our_address: Option<String>,
    ) -> (Self, mpsc::UnboundedReceiver<ManagerEvent>) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (manager_tx, manager_rx) = mpsc::unbounded_channel();

        (
            Self {
                our_peer_id,
                our_address,
                connections: HashMap::new(),
                peer_to_conn: HashMap::new(),
                next_conn_id: 1,
                event_tx,
                event_rx,
                manager_tx,
                reconnect_config: ReconnectConfig::default(),
            },
            manager_rx,
        )
    }

    /// Get our peer ID.
    pub fn peer_id(&self) -> &str {
        &self.our_peer_id
    }

    /// Get our advertised address.
    pub fn address(&self) -> Option<&str> {
        self.our_address.as_deref()
    }

    /// Handle a new incoming TCP connection.
    ///
    /// Upgrades to WebSocket and sends our handshake.
    pub async fn accept_incoming(&mut self, stream: TcpStream, addr: SocketAddr) {
        // Upgrade to WebSocket
        let ws_stream = match accept_async(stream).await {
            Ok(ws) => ws,
            Err(e) => {
                let err_str = e.to_string();
                if err_str.contains("Handshake not finished")
                    || err_str.contains("Connection reset")
                    || err_str.contains("unexpected EOF")
                {
                    debug!("Connection closed before handshake from {}", addr);
                } else {
                    error!("WebSocket upgrade failed for {}: {}", addr, e);
                }
                return;
            }
        };

        // Generate temp ID
        let temp_id = format!("peer-{}", self.next_conn_id);
        self.next_conn_id += 1;

        info!("New incoming connection from {} (temp_id: {})", addr, temp_id);

        // Create connection
        let conn = PeerConnection::new(temp_id.clone(), ws_stream, self.event_tx.clone());

        // Send our handshake immediately (include our address if we have one)
        if let Err(e) = conn.send_handshake(&self.our_peer_id, self.our_address.as_deref()).await {
            error!("Failed to send handshake to {}: {}", temp_id, e);
            return;
        }

        // Store connection
        self.connections
            .insert(temp_id, Connection::Incoming(conn));
    }

    /// Connect to a remote peer.
    ///
    /// Establishes WebSocket connection and sends handshake.
    pub async fn connect_to(&mut self, address: &str) -> Result<()> {
        // Check if we already have a connection to this address
        if self.connections.contains_key(address) {
            debug!("Already connected to {}", address);
            return Ok(());
        }

        info!("Connecting to {}", address);

        let mut conn = OutgoingConnection::new(
            address.to_string(),
            self.our_peer_id.clone(),
            self.our_address.clone(),
        );
        conn.connect(self.event_tx.clone()).await?;

        self.connections
            .insert(address.to_string(), Connection::Outgoing(conn));

        Ok(())
    }

    /// Process internal connection events.
    ///
    /// Call this from the event loop to handle handshakes, messages, and closes.
    pub async fn poll_events(&mut self) -> Option<ManagerEvent> {
        let event = self.event_rx.recv().await?;

        match event {
            ConnectionEvent::Handshake {
                temp_id,
                peer_id,
                address,
            } => self.on_handshake(&temp_id, &peer_id, address).await,
            ConnectionEvent::Message(mut msg) => {
                // Resolve conn_id → peer_id so callers see real peer IDs
                if let Some(pid) = self.resolve_peer_id(&msg.temp_id) {
                    msg.temp_id = pid;
                }
                Some(ManagerEvent::Message(msg))
            }
            ConnectionEvent::Closed { temp_id } => self.on_closed(&temp_id).await,
        }
    }

    /// Handle handshake completion.
    async fn on_handshake(
        &mut self,
        conn_id: &str,
        peer_id: &str,
        address: Option<String>,
    ) -> Option<ManagerEvent> {
        let conn = self.connections.get(conn_id)?;
        let direction = conn.direction();

        // Check for duplicate connection
        let existing_direction = self
            .peer_to_conn
            .get(peer_id)
            .and_then(|id| self.connections.get(id))
            .map(|c| c.direction());

        let dedup_result =
            check_duplicate_connection(&self.our_peer_id, peer_id, direction, existing_direction);

        match dedup_result {
            DuplicateCheckResult::NoDuplicate => {
                // Normal handshake completion
                self.peer_to_conn
                    .insert(peer_id.to_string(), conn_id.to_string());

                // Update connection's peer ID
                match self.connections.get_mut(conn_id) {
                    Some(Connection::Incoming(c)) => c.set_peer_id(peer_id.to_string()),
                    Some(Connection::Outgoing(c)) => c.on_handshake_complete(peer_id.to_string()),
                    None => {}
                }

                debug!("Handshake complete: {} -> {}", conn_id, peer_id);
                Some(ManagerEvent::HandshakeComplete {
                    connection_id: conn_id.to_string(),
                    peer_id: peer_id.to_string(),
                    direction,
                    address: address.clone(),
                })
            }
            DuplicateCheckResult::CloseThis => {
                // Close this connection (the new one)
                info!(
                    "Duplicate connection detected, closing {} (keeping existing)",
                    conn_id
                );
                self.close_connection(conn_id, DisconnectReason::DuplicateConnection)
                    .await;
                None
            }
            DuplicateCheckResult::CloseOther => {
                // Close the other connection - look up its connection_id from peer_to_conn
                if let Some(existing_conn_id) = self.peer_to_conn.get(peer_id).cloned() {
                    info!(
                        "Duplicate connection detected, closing {} (keeping new)",
                        existing_conn_id
                    );
                    self.close_connection(&existing_conn_id, DisconnectReason::DuplicateConnection)
                        .await;
                }

                // Register the new connection
                self.peer_to_conn
                    .insert(peer_id.to_string(), conn_id.to_string());
                match self.connections.get_mut(conn_id) {
                    Some(Connection::Incoming(c)) => c.set_peer_id(peer_id.to_string()),
                    Some(Connection::Outgoing(c)) => c.on_handshake_complete(peer_id.to_string()),
                    None => {}
                }

                Some(ManagerEvent::HandshakeComplete {
                    connection_id: conn_id.to_string(),
                    peer_id: peer_id.to_string(),
                    direction,
                    address,
                })
            }
        }
    }

    /// Handle connection closed.
    async fn on_closed(&mut self, conn_id: &str) -> Option<ManagerEvent> {
        let conn = self.connections.remove(conn_id)?;
        let peer_id = conn.peer_id().map(|s| s.to_string());
        let direction = conn.direction();

        // Clean up peer_to_conn mapping
        if let Some(ref pid) = peer_id {
            self.peer_to_conn.remove(pid);
        }

        let pid_for_event = peer_id.unwrap_or_else(|| conn_id.to_string());

        // For outgoing connections, schedule reconnection
        if let Connection::Outgoing(mut outgoing) = conn {
            if outgoing.state != OutgoingState::Closed {
                let now_ms = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_millis() as u64;

                outgoing.prepare_reconnect(now_ms, &self.reconnect_config);
                self.connections
                    .insert(outgoing.address.clone(), Connection::Outgoing(outgoing));
            }
        }

        info!(
            "Connection closed: {} ({:?})",
            pid_for_event, direction
        );

        Some(ManagerEvent::ConnectionClosed {
            peer_id: pid_for_event,
            reason: DisconnectReason::RemoteClosed,
        })
    }

    /// Close a connection.
    async fn close_connection(&mut self, conn_id: &str, reason: DisconnectReason) {
        if let Some(mut conn) = self.connections.remove(conn_id) {
            // Clean up mappings
            if let Some(pid) = conn.peer_id() {
                self.peer_to_conn.remove(pid);
            }

            // Close the actual connection
            match &mut conn {
                Connection::Incoming(c) => c.close().await,
                Connection::Outgoing(c) => c.close().await,
            }

            let _ = self.manager_tx.send(ManagerEvent::ConnectionClosed {
                peer_id: conn.peer_id().unwrap_or(conn_id).to_string(),
                reason,
            });
        }
    }

    /// Send data to a specific peer.
    pub async fn send(&self, peer_id: &str, data: &[u8]) -> Result<()> {
        let conn_id = self
            .peer_to_conn
            .get(peer_id)
            .ok_or_else(|| anyhow::anyhow!("Unknown peer: {}", peer_id))?;

        let conn = self
            .connections
            .get(conn_id)
            .ok_or_else(|| anyhow::anyhow!("Connection not found: {}", conn_id))?;

        conn.send(data).await
    }

    /// Broadcast data to all connected peers.
    pub async fn broadcast(&self, data: &[u8]) {
        for (conn_id, conn) in &self.connections {
            if conn.peer_id().is_some() {
                if let Err(e) = conn.send(data).await {
                    warn!("Failed to broadcast to {}: {}", conn_id, e);
                }
            }
        }
    }

    /// Broadcast data to all connected peers except one.
    pub async fn broadcast_except(&self, data: &[u8], exclude_peer_id: &str) {
        for (conn_id, conn) in &self.connections {
            if let Some(pid) = conn.peer_id() {
                if pid != exclude_peer_id {
                    if let Err(e) = conn.send(data).await {
                        warn!("Failed to broadcast to {}: {}", conn_id, e);
                    }
                }
            }
        }
    }

    /// Get the number of connected peers (with completed handshake).
    pub fn peer_count(&self) -> usize {
        self.connections
            .values()
            .filter(|c| c.peer_id().is_some())
            .count()
    }

    /// Get list of connected peer IDs.
    pub fn connected_peers(&self) -> Vec<String> {
        self.connections
            .values()
            .filter_map(|c| c.peer_id().map(|s| s.to_string()))
            .collect()
    }

    /// Check for outgoing connections that need reconnection.
    ///
    /// Returns addresses that should be reconnected.
    pub fn check_reconnections(&self, now_ms: u64) -> Vec<String> {
        self.connections
            .values()
            .filter_map(|conn| {
                if let Connection::Outgoing(out) = conn {
                    if out.should_reconnect(now_ms) {
                        return Some(out.address.clone());
                    }
                }
                None
            })
            .collect()
    }

    /// Resolve a connection ID to peer ID.
    pub fn resolve_peer_id(&self, conn_id: &str) -> Option<String> {
        self.connections.get(conn_id).and_then(|c| c.peer_id()).map(|s| s.to_string())
    }

    /// Check if connected to a specific peer.
    pub fn is_connected(&self, peer_id: &str) -> bool {
        self.peer_to_conn.contains_key(peer_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_manager_creation() {
        let (manager, _rx) =
            ConnectionManager::new("our-peer".into(), Some("ws://localhost:8080".into()));

        assert_eq!(manager.peer_id(), "our-peer");
        assert_eq!(manager.address(), Some("ws://localhost:8080"));
        assert_eq!(manager.peer_count(), 0);
    }

    #[test]
    fn test_manager_client_only() {
        let (manager, _rx) = ConnectionManager::new("our-peer".into(), None);

        assert!(manager.address().is_none());
    }

    #[test]
    fn test_connected_peers_empty() {
        let (manager, _rx) = ConnectionManager::new("our-peer".into(), None);
        assert!(manager.connected_peers().is_empty());
    }

    #[test]
    fn test_is_connected_empty() {
        let (manager, _rx) = ConnectionManager::new("our-peer".into(), None);
        assert!(!manager.is_connected("other-peer"));
    }

    // Note: Full integration tests require actual WebSocket connections,
    // which are better suited for e2e tests in tests/e2e.rs
}
