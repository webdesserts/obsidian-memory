//! Individual peer connection management.
//!
//! Each peer connection wraps a WebSocket stream, handling the split
//! between read and write halves for async operation.

use crate::message::{HandshakeMessage, MAX_MESSAGE_SIZE};
use anyhow::{anyhow, Result};
use futures::{SinkExt, StreamExt};
use std::sync::Arc;
use tokio::net::TcpStream;
use tokio::sync::{mpsc, Mutex};
use tokio::task::JoinHandle;
use tokio_tungstenite::{
    tungstenite::{Error as WsError, Message},
    WebSocketStream,
};
use tracing::{debug, error, warn};

/// Message received from a peer connection.
#[derive(Debug)]
pub struct IncomingMessage {
    /// Temporary connection ID (e.g., "peer-1")
    pub temp_id: String,
    /// Raw message data
    pub data: Vec<u8>,
}

/// Event emitted by a connection.
#[derive(Debug)]
pub enum ConnectionEvent {
    /// Received a message from the peer
    Message(IncomingMessage),
    /// Peer completed handshake, revealing their real peer ID
    Handshake {
        temp_id: String,
        peer_id: String,
        address: Option<String>,
    },
    /// Connection was closed
    Closed { temp_id: String },
}

/// A single WebSocket connection to a peer.
pub struct PeerConnection {
    /// Temporary ID assigned by server (e.g., "peer-1")
    pub temp_id: String,
    /// Real peer ID (known after handshake)
    pub real_peer_id: Option<String>,
    /// Write half of the WebSocket (wrapped for sharing across tasks)
    write: Arc<Mutex<futures::stream::SplitSink<WebSocketStream<TcpStream>, Message>>>,
    /// Handle to the read task
    read_task: Option<JoinHandle<()>>,
}

impl PeerConnection {
    /// Create a new peer connection from a WebSocket stream.
    ///
    /// Spawns a read task that forwards messages to the event channel.
    pub fn new(
        temp_id: String,
        ws_stream: WebSocketStream<TcpStream>,
        event_tx: mpsc::UnboundedSender<ConnectionEvent>,
    ) -> Self {
        let (write, read) = ws_stream.split();
        let write = Arc::new(Mutex::new(write));

        let read_temp_id = temp_id.clone();
        let read_task = tokio::spawn(async move {
            Self::read_loop(read_temp_id, read, event_tx).await;
        });

        Self {
            temp_id,
            real_peer_id: None,
            write,
            read_task: Some(read_task),
        }
    }

    /// Read loop that forwards messages to the event channel.
    async fn read_loop(
        temp_id: String,
        mut read: futures::stream::SplitStream<WebSocketStream<TcpStream>>,
        event_tx: mpsc::UnboundedSender<ConnectionEvent>,
    ) {
        loop {
            match read.next().await {
                Some(Ok(msg)) => {
                    let data = match msg {
                        Message::Binary(data) => data,
                        Message::Text(text) => text.into_bytes(),
                        Message::Ping(_) | Message::Pong(_) => continue,
                        Message::Close(_) => {
                            debug!("Received close frame from {}", temp_id);
                            break;
                        }
                        Message::Frame(_) => continue,
                    };

                    // Check message size
                    if data.len() > MAX_MESSAGE_SIZE {
                        warn!(
                            "Message from {} exceeds max size ({} > {}), dropping",
                            temp_id,
                            data.len(),
                            MAX_MESSAGE_SIZE
                        );
                        continue;
                    }

                    // Check if this is a handshake message
                    debug!(
                        "Message from {}: {} bytes, starts_with_brace={}",
                        temp_id,
                        data.len(),
                        data.first() == Some(&b'{')
                    );
                    if let Some(handshake) = HandshakeMessage::from_binary(&data) {
                        debug!(
                            "Received handshake from {} (peer_id: {}, role: {}, address: {:?})",
                            temp_id, handshake.peer_id, handshake.role, handshake.address
                        );
                        let _ = event_tx.send(ConnectionEvent::Handshake {
                            temp_id: temp_id.clone(),
                            peer_id: handshake.peer_id,
                            address: handshake.address,
                        });
                    } else {
                        // Regular sync message
                        let _ = event_tx.send(ConnectionEvent::Message(IncomingMessage {
                            temp_id: temp_id.clone(),
                            data,
                        }));
                    }
                }
                Some(Err(e)) => {
                    match e {
                        WsError::ConnectionClosed | WsError::AlreadyClosed => {
                            debug!("Connection {} closed", temp_id);
                        }
                        _ => {
                            error!("WebSocket error on {}: {}", temp_id, e);
                        }
                    }
                    break;
                }
                None => {
                    debug!("Connection {} stream ended", temp_id);
                    break;
                }
            }
        }

        // Notify that connection is closed
        let _ = event_tx.send(ConnectionEvent::Closed {
            temp_id: temp_id.clone(),
        });
    }

    /// Send binary data to the peer.
    ///
    /// All messages are sent as binary WebSocket frames.
    pub async fn send(&self, data: &[u8]) -> Result<()> {
        let mut write = self.write.lock().await;
        write
            .send(Message::Binary(data.to_vec().into()))
            .await
            .map_err(|e| anyhow!("Failed to send message: {}", e))
    }

    /// Send a handshake message to the peer, optionally including our address.
    pub async fn send_handshake(&self, peer_id: &str, address: Option<&str>) -> Result<()> {
        let handshake = match address {
            Some(addr) => HandshakeMessage::with_address(peer_id, "server", addr),
            None => HandshakeMessage::new(peer_id, "server"),
        };
        self.send(&handshake.to_binary()).await
    }

    /// Set the real peer ID after handshake.
    pub fn set_peer_id(&mut self, peer_id: String) {
        self.real_peer_id = Some(peer_id);
    }

    /// Close the connection gracefully.
    pub async fn close(&mut self) {
        // Send close frame
        if let Ok(mut write) = self.write.try_lock() {
            let _ = write.send(Message::Close(None)).await;
        }

        // Abort the read task
        if let Some(task) = self.read_task.take() {
            task.abort();
        }
    }
}

impl Drop for PeerConnection {
    fn drop(&mut self) {
        if let Some(task) = self.read_task.take() {
            task.abort();
        }
    }
}
