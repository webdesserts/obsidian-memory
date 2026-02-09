//! WebSocket server for accepting peer connections.
//!
//! Manages connection lifecycle, peer ID mapping, and message routing.
//! The handshake lifecycle is encapsulated: callers only see `ServerEvent`s
//! with resolved peer IDs via `poll_event()`.

use crate::connection::{ConnectionEvent, IncomingMessage, PeerConnection};
use anyhow::Result;
use std::collections::HashMap;
use std::net::SocketAddr;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio_tungstenite::accept_async;
use tracing::{debug, error, info, warn};

/// Event emitted by the server after the handshake lifecycle is resolved.
/// Callers only see peer IDs — temp IDs are an internal detail.
#[derive(Debug)]
pub enum ServerEvent {
    /// A peer completed handshake and is now connected.
    PeerConnected {
        peer_id: String,
        address: Option<String>,
    },
    /// A message from an identified peer.
    Message(IncomingMessage),
    /// A previously-connected peer disconnected.
    PeerDisconnected { peer_id: String },
}

/// WebSocket server managing peer connections.
pub struct WebSocketServer {
    /// Our peer ID
    peer_id: String,
    /// Our advertised address (None = client-only)
    our_address: Option<String>,
    /// Pre-handshake connections indexed by temp ID
    connections: HashMap<String, PeerConnection>,
    /// Post-handshake peers indexed by peer ID
    peers: HashMap<String, PeerConnection>,
    /// Map from connection ID (temp_id) to real peer ID
    conn_id_to_peer: HashMap<String, String>,
    /// Map from real peer ID to temp ID
    peer_to_temp: HashMap<String, String>,
    /// Map from temp ID to real peer ID
    temp_to_peer: HashMap<String, String>,
    /// Counter for generating temp IDs
    next_conn_id: u64,
    /// Channel sender for connection events (messages, handshakes, closes)
    event_tx: mpsc::UnboundedSender<ConnectionEvent>,
    /// Channel receiver for connection events
    event_rx: mpsc::UnboundedReceiver<ConnectionEvent>,
    /// Channel for notifying when a peer completes handshake (peer_id, address)
    peer_connected_tx: mpsc::UnboundedSender<(String, Option<String>)>,
}

impl WebSocketServer {
    /// Create a new WebSocket server.
    pub fn new(
        peer_id: String,
        our_address: Option<String>,
    ) -> (Self, mpsc::UnboundedReceiver<(String, Option<String>)>) {
        let (event_tx, event_rx) = mpsc::unbounded_channel();
        let (peer_connected_tx, peer_connected_rx) = mpsc::unbounded_channel();

        (
            Self {
                peer_id,
                our_address,
                connections: HashMap::new(),
                peers: HashMap::new(),
                conn_id_to_peer: HashMap::new(),
                peer_to_temp: HashMap::new(),
                temp_to_peer: HashMap::new(),
                next_conn_id: 1,
                event_tx,
                event_rx,
                peer_connected_tx,
            },
            peer_connected_rx,
        )
    }

    /// Bind to an address and return the TCP listener.
    pub async fn bind(listen_addr: &str) -> Result<TcpListener> {
        let listener = TcpListener::bind(listen_addr).await?;
        info!("WebSocket server listening on {}", listen_addr);
        Ok(listener)
    }

    /// Handle a new incoming TCP connection.
    ///
    /// Upgrades to WebSocket and sends our handshake.
    pub async fn accept_connection(&mut self, stream: TcpStream, addr: SocketAddr) {
        // Upgrade to WebSocket
        let ws_stream = match accept_async(stream).await {
            Ok(ws) => ws,
            Err(e) => {
                // Health checks (like `nc -z`) connect and immediately close without
                // completing the WebSocket handshake. Log these as debug, not error.
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

        info!("New connection from {} (temp_id: {})", addr, temp_id);

        // Create connection
        let conn = PeerConnection::new(temp_id.clone(), ws_stream, self.event_tx.clone());

        // Send our handshake immediately (include our address if we have one)
        if let Err(e) = conn.send_handshake(&self.peer_id, self.our_address.as_deref()).await {
            error!("Failed to send handshake to {}: {}", temp_id, e);
            return;
        }

        // Store connection
        self.connections.insert(temp_id, conn);
    }

    /// Wait for the next connection event.
    pub async fn recv_event(&mut self) -> Option<ConnectionEvent> {
        self.event_rx.recv().await
    }

    /// Wait for the next server event, encapsulating the handshake lifecycle.
    ///
    /// Pre-handshake connections are an internal detail. Callers only see:
    /// - `PeerConnected` when a handshake completes
    /// - `Message` with the resolved peer ID
    /// - `PeerDisconnected` when a post-handshake peer disconnects
    ///
    /// Connections that close before handshake are silently cleaned up.
    pub async fn poll_event(&mut self) -> Option<ServerEvent> {
        loop {
            let event = self.event_rx.recv().await?;

            match event {
                ConnectionEvent::Handshake {
                    temp_id,
                    peer_id,
                    address,
                } => {
                    debug!(
                        "Handshake complete: {} is now known as {} (address: {:?})",
                        temp_id, peer_id, address
                    );

                    // Move connection from pre-handshake to post-handshake
                    if let Some(mut conn) = self.connections.remove(&temp_id) {
                        conn.set_peer_id(peer_id.clone());
                        self.peers.insert(peer_id.clone(), conn);
                    }

                    // Update ID mappings
                    self.conn_id_to_peer
                        .insert(temp_id.clone(), peer_id.clone());

                    // Also update legacy maps (still used by recv_event callers)
                    self.peer_to_temp
                        .insert(peer_id.clone(), temp_id.to_string());
                    self.temp_to_peer
                        .insert(temp_id.to_string(), peer_id.clone());

                    // Notify legacy peer_connected channel
                    let _ = self
                        .peer_connected_tx
                        .send((peer_id.clone(), address.clone()));

                    return Some(ServerEvent::PeerConnected { peer_id, address });
                }
                ConnectionEvent::Message(mut msg) => {
                    // Resolve temp_id → peer_id
                    if let Some(peer_id) = self.conn_id_to_peer.get(&msg.temp_id) {
                        msg.temp_id = peer_id.clone();
                    }
                    return Some(ServerEvent::Message(msg));
                }
                ConnectionEvent::Closed { temp_id } => {
                    if let Some(peer_id) = self.conn_id_to_peer.remove(&temp_id) {
                        // Post-handshake: clean up and emit event
                        self.peers.remove(&peer_id);
                        self.peer_to_temp.remove(&peer_id);
                        self.temp_to_peer.remove(&temp_id);
                        // Also remove from legacy connections map
                        self.connections.remove(&temp_id);

                        return Some(ServerEvent::PeerDisconnected { peer_id });
                    } else {
                        // Pre-handshake: silent cleanup, continue loop
                        self.connections.remove(&temp_id);
                        debug!(
                            "Connection closed before handshake: {}, not emitting event",
                            temp_id
                        );
                        continue;
                    }
                }
            }
        }
    }

    /// Register a peer after handshake completion.
    pub fn register_peer(&mut self, temp_id: &str, peer_id: String, address: Option<String>) {
        debug!(
            "Handshake complete: {} is now known as {} (address: {:?})",
            temp_id, peer_id, address
        );

        // Update ID mappings
        self.peer_to_temp.insert(peer_id.clone(), temp_id.to_string());
        self.temp_to_peer.insert(temp_id.to_string(), peer_id.clone());

        // Update connection's real peer ID
        if let Some(conn) = self.connections.get_mut(temp_id) {
            conn.set_peer_id(peer_id.clone());
        }

        // Notify that peer is connected (for sync initiation)
        let _ = self.peer_connected_tx.send((peer_id, address));
    }

    /// Remove a peer after connection close.
    pub fn remove_peer(&mut self, temp_id: &str) {
        // Clean up ID mappings
        if let Some(peer_id) = self.temp_to_peer.remove(temp_id) {
            self.peer_to_temp.remove(&peer_id);
        }

        // Remove connection
        self.connections.remove(temp_id);
    }

    /// Send data to a specific peer by their real peer ID.
    pub async fn send(&self, peer_id: &str, data: &[u8]) -> Result<()> {
        // Try new peers map first, fall back to legacy lookup
        if let Some(conn) = self.peers.get(peer_id) {
            return conn.send(data).await;
        }

        let temp_id = self
            .peer_to_temp
            .get(peer_id)
            .ok_or_else(|| anyhow::anyhow!("Unknown peer: {}", peer_id))?;

        let conn = self
            .connections
            .get(temp_id)
            .ok_or_else(|| anyhow::anyhow!("Connection not found: {}", temp_id))?;

        conn.send(data).await
    }

    /// Send data to a peer by temp ID (used before handshake completes).
    pub async fn send_by_temp_id(&self, temp_id: &str, data: &[u8]) -> Result<()> {
        let conn = self
            .connections
            .get(temp_id)
            .ok_or_else(|| anyhow::anyhow!("Connection not found: {}", temp_id))?;

        conn.send(data).await
    }

    /// Broadcast data to all connected peers.
    pub async fn broadcast(&self, data: &[u8]) {
        // Use peers map (post-handshake connections)
        for (peer_id, conn) in &self.peers {
            if let Err(e) = conn.send(data).await {
                warn!("Failed to broadcast to {}: {}", peer_id, e);
            }
        }
        // Also check legacy connections map for peers registered via recv_event path
        for (temp_id, conn) in &self.connections {
            if let Some(ref pid) = conn.real_peer_id {
                if !self.peers.contains_key(pid) {
                    if let Err(e) = conn.send(data).await {
                        warn!("Failed to broadcast to {}: {}", temp_id, e);
                    }
                }
            }
        }
    }

    /// Broadcast data to all connected peers except one.
    ///
    /// The `exclude_peer_id` parameter is compared against real peer IDs,
    /// not temp IDs. This fixes the bug where the sender was never excluded.
    pub async fn broadcast_except(&self, data: &[u8], exclude_peer_id: &str) {
        // Use peers map (post-handshake connections)
        for (peer_id, conn) in &self.peers {
            if peer_id == exclude_peer_id {
                continue;
            }
            if let Err(e) = conn.send(data).await {
                warn!("Failed to relay to {}: {}", peer_id, e);
            }
        }
        // Also check legacy connections map for peers registered via recv_event path
        for (temp_id, conn) in &self.connections {
            if let Some(ref pid) = conn.real_peer_id {
                if pid == exclude_peer_id {
                    continue;
                }
                if !self.peers.contains_key(pid) {
                    if let Err(e) = conn.send(data).await {
                        warn!("Failed to relay to {}: {}", temp_id, e);
                    }
                }
            }
        }
    }

    /// Get the number of connected peers (with completed handshake).
    pub fn peer_count(&self) -> usize {
        // Count from peers map + legacy connections with completed handshake
        let legacy_count = self
            .connections
            .values()
            .filter(|c| {
                c.real_peer_id
                    .as_ref()
                    .map(|pid| !self.peers.contains_key(pid))
                    .unwrap_or(false)
            })
            .count();
        self.peers.len() + legacy_count
    }

    /// Get list of connected peer IDs.
    pub fn connected_peers(&self) -> Vec<String> {
        let mut ids: Vec<String> = self.peers.keys().cloned().collect();
        // Include legacy connections not yet in peers map
        for conn in self.connections.values() {
            if let Some(ref pid) = conn.real_peer_id {
                if !self.peers.contains_key(pid) {
                    ids.push(pid.clone());
                }
            }
        }
        ids
    }

    /// Resolve a temp ID to real peer ID if known.
    pub fn resolve_peer_id(&self, temp_id: &str) -> Option<String> {
        self.conn_id_to_peer
            .get(temp_id)
            .cloned()
            .or_else(|| self.temp_to_peer.get(temp_id).cloned())
    }
}
