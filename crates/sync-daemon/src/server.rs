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
    /// Pre-handshake connections indexed by conn_id
    pending: HashMap<String, PeerConnection>,
    /// Post-handshake peers indexed by peer ID
    peers: HashMap<String, PeerConnection>,
    /// Map from conn_id to real peer ID (for resolving messages/closes)
    conn_id_to_peer: HashMap<String, String>,
    /// Counter for generating connection IDs
    next_conn_id: u64,
    /// Channel sender for connection events (messages, handshakes, closes)
    event_tx: mpsc::UnboundedSender<ConnectionEvent>,
    /// Channel receiver for connection events
    event_rx: mpsc::UnboundedReceiver<ConnectionEvent>,
}

impl WebSocketServer {
    /// Create a new WebSocket server.
    pub fn new(peer_id: String, our_address: Option<String>) -> Self {
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        Self {
            peer_id,
            our_address,
            pending: HashMap::new(),
            peers: HashMap::new(),
            conn_id_to_peer: HashMap::new(),
            next_conn_id: 1,
            event_tx,
            event_rx,
        }
    }

    /// Bind to an address and return the TCP listener.
    pub async fn bind(listen_addr: &str) -> Result<TcpListener> {
        let listener = TcpListener::bind(listen_addr).await?;
        info!("WebSocket server listening on {}", listen_addr);
        Ok(listener)
    }

    /// Handle a new incoming TCP connection.
    ///
    /// Upgrades to WebSocket and sends our handshake. The connection stays
    /// in the pending map until the remote peer completes handshake.
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

        // Generate internal connection ID
        let conn_id = format!("conn-{}", self.next_conn_id);
        self.next_conn_id += 1;

        info!("New connection from {} (conn_id: {})", addr, conn_id);

        // Create connection
        let conn = PeerConnection::new(conn_id.clone(), ws_stream, self.event_tx.clone());

        // Send our handshake immediately (include our address if we have one)
        if let Err(e) = conn
            .send_handshake(&self.peer_id, self.our_address.as_deref())
            .await
        {
            error!("Failed to send handshake to {}: {}", conn_id, e);
            return;
        }

        // Store in pending until handshake completes
        self.pending.insert(conn_id, conn);
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
                    conn_id,
                    peer_id,
                    address,
                } => {
                    debug!(
                        "Handshake complete: {} is now known as {} (address: {:?})",
                        conn_id, peer_id, address
                    );

                    // Move connection from pending to peers
                    if let Some(mut conn) = self.pending.remove(&conn_id) {
                        conn.set_peer_id(peer_id.clone());
                        self.peers.insert(peer_id.clone(), conn);
                    }

                    self.conn_id_to_peer
                        .insert(conn_id, peer_id.clone());

                    return Some(ServerEvent::PeerConnected { peer_id, address });
                }
                ConnectionEvent::Message(mut msg) => {
                    // Resolve conn_id → peer_id
                    if let Some(peer_id) = self.conn_id_to_peer.get(&msg.peer_id) {
                        msg.peer_id = peer_id.clone();
                    }
                    return Some(ServerEvent::Message(msg));
                }
                ConnectionEvent::Closed { conn_id } => {
                    if let Some(peer_id) = self.conn_id_to_peer.remove(&conn_id) {
                        // Post-handshake: clean up and emit event
                        self.peers.remove(&peer_id);
                        return Some(ServerEvent::PeerDisconnected { peer_id });
                    } else {
                        // Pre-handshake: silent cleanup, continue loop
                        self.pending.remove(&conn_id);
                        debug!(
                            "Connection closed before handshake: {}, not emitting event",
                            conn_id
                        );
                        continue;
                    }
                }
            }
        }
    }

    /// Send data to a specific peer by their real peer ID.
    pub async fn send(&self, peer_id: &str, data: &[u8]) -> Result<()> {
        let conn = self
            .peers
            .get(peer_id)
            .ok_or_else(|| anyhow::anyhow!("Unknown peer: {}", peer_id))?;

        conn.send(data).await
    }

    /// Broadcast data to all connected peers.
    pub async fn broadcast(&self, data: &[u8]) {
        for (peer_id, conn) in &self.peers {
            if let Err(e) = conn.send(data).await {
                warn!("Failed to broadcast to {}: {}", peer_id, e);
            }
        }
    }

    /// Broadcast data to all connected peers except one.
    pub async fn broadcast_except(&self, data: &[u8], exclude_peer_id: &str) {
        for (peer_id, conn) in &self.peers {
            if peer_id == exclude_peer_id {
                continue;
            }
            if let Err(e) = conn.send(data).await {
                warn!("Failed to relay to {}: {}", peer_id, e);
            }
        }
    }

    /// Get the number of connected peers (with completed handshake).
    pub fn peer_count(&self) -> usize {
        self.peers.len()
    }

    /// Get list of connected peer IDs.
    pub fn connected_peers(&self) -> Vec<String> {
        self.peers.keys().cloned().collect()
    }
}
