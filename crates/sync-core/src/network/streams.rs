//! QUIC bi-stream sync protocol handler.
//!
//! `SyncStreamHandler` implements iroh's `ProtocolHandler` trait, which means
//! the Router dispatches incoming connections on `SYNC_ALPN` directly to it.
//!
//! # Protocol flow
//!
//! **Inbound (peer opens a connection to us):**
//! 1. Peer opens a QUIC connection with `SYNC_ALPN`.
//! 2. Peer opens a bi-directional stream and sends a [`SyncMessage`].
//! 3. We deserialize the message and send it to the event channel so the
//!    vault layer can process it.
//! 4. The vault layer writes the response back via the provided one-shot channel.
//! 5. We serialize the response and write it to the stream.
//!
//! **Outbound (we initiate sync with a peer on `NeighborUp`):**
//! Call [`connect_and_sync`] to open a bi-stream to the peer, send a
//! [`SyncMessage`], and receive a [`SyncMessage`] back.
//!
//! This handler intentionally does *not* contain vault logic. It is a thin
//! transport layer that serializes/deserializes messages and passes them to the
//! caller via an async channel.

use std::sync::Arc;

use anyhow::Result;
use iroh::endpoint::{Connection, RecvStream, SendStream};
use iroh::protocol::{AcceptError, ProtocolHandler};
use tokio::sync::mpsc;
use tracing::{debug, warn};

use crate::sync::SyncMessage;

/// Maximum byte length for a framed sync message.
///
/// 64 MiB covers a realistic worst case of syncing a large vault in one batch.
/// Messages larger than this are rejected to prevent memory exhaustion.
const MAX_MESSAGE_BYTES: u32 = 64 * 1024 * 1024;

/// An inbound sync request received from a remote peer.
///
/// The caller processes the message (via the vault sync engine) and sends the
/// response back through `reply_tx`. The handler blocks the QUIC stream until
/// the response is sent or `reply_tx` is dropped (which closes the stream
/// without a response).
pub struct InboundSyncRequest {
    /// The message received from the remote peer.
    pub message: SyncMessage,
    /// Send the response back through here.
    pub reply_tx: tokio::sync::oneshot::Sender<SyncMessage>,
}

/// Receives inbound sync requests from remote peers.
///
/// Produced by [`SyncStreamHandler::new`]. Drive this receiver in a task to
/// process incoming sync requests from other nodes.
pub type InboundSyncRx = mpsc::UnboundedReceiver<InboundSyncRequest>;

/// QUIC bi-stream sync protocol handler.
///
/// Registered with iroh's `Router` for the `SYNC_ALPN`. When a remote peer
/// connects, the router calls `accept()` here, which reads one `SyncMessage`
/// from the stream and forwards it to the event channel.
///
/// Clone-safe — cloning shares the same underlying channel sender.
#[derive(Debug, Clone)]
pub struct SyncStreamHandler {
    /// Sends inbound requests to the vault layer for processing.
    inbound_tx: Arc<mpsc::UnboundedSender<InboundSyncRequest>>,
}

impl SyncStreamHandler {
    /// Create a new handler and return the inbound event receiver.
    ///
    /// Drive the returned receiver in a task to process sync requests.
    pub fn new() -> (Self, InboundSyncRx) {
        let (tx, rx) = mpsc::unbounded_channel();
        (
            Self {
                inbound_tx: Arc::new(tx),
            },
            rx,
        )
    }
}

impl ProtocolHandler for SyncStreamHandler {
    /// Accept an incoming QUIC connection on `SYNC_ALPN`.
    ///
    /// Accepts one bi-stream, reads a `SyncMessage`, forwards it to the
    /// inbound channel, and writes the response from the vault layer back.
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let (mut send, mut recv) = connection
            .accept_bi()
            .await
            .map_err(AcceptError::from_err)?;

        let message = read_message(&mut recv)
            .await
            .map_err(|e| AcceptError::from_boxed(e.into()))?;
        debug!("Inbound sync message: {:?}", std::mem::discriminant(&message));

        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let request = InboundSyncRequest { message, reply_tx };

        if self.inbound_tx.send(request).is_err() {
            // No one is listening for inbound requests — close the stream.
            warn!("Inbound sync request dropped: no receiver");
            return Ok(());
        }

        // Wait for the vault layer to respond.
        match reply_rx.await {
            Ok(response) => {
                write_message(&mut send, &response)
                    .await
                    .map_err(|e| AcceptError::from_boxed(e.into()))?;
                send.finish().map_err(AcceptError::from_err)?;
            }
            Err(_) => {
                // reply_tx was dropped without a response — close without writing.
                debug!("Inbound sync reply_tx dropped without response");
            }
        }

        // Wait for the client to close the connection. Without this, the connection
        // is dropped when accept() returns, potentially before the client finishes
        // reading the response we just sent.
        connection.closed().await;

        Ok(())
    }
}

/// Open a QUIC bi-stream to `peer`, send `request`, and return the response.
///
/// Used when initiating sync after a `NeighborUp` gossip event. The caller
/// provides the `EndpointAddr` (or `EndpointId`) to dial.
pub async fn connect_and_sync(
    endpoint: &iroh::Endpoint,
    peer: impl Into<iroh::EndpointAddr>,
    request: SyncMessage,
) -> Result<SyncMessage> {
    let alpn = super::node::SYNC_ALPN;
    let connection = endpoint.connect(peer, alpn).await?;
    let (mut send, mut recv) = connection.open_bi().await?;

    write_message(&mut send, &request).await?;
    send.finish()?;

    let response = read_message(&mut recv).await?;
    Ok(response)
}

/// Write a length-prefixed, bincode-encoded `SyncMessage` to the stream.
///
/// Format: `[u32 little-endian length][bincode bytes]`
async fn write_message(send: &mut SendStream, message: &SyncMessage) -> Result<()> {
    let encoded = bincode::serialize(message)?;
    let len = u32::try_from(encoded.len())
        .map_err(|_| anyhow::anyhow!("Message too large to frame: {} bytes", encoded.len()))?;
    send.write_all(&len.to_le_bytes()).await?;
    send.write_all(&encoded).await?;
    Ok(())
}

/// Read a length-prefixed, bincode-encoded `SyncMessage` from the stream.
///
/// Format: `[u32 little-endian length][bincode bytes]`
async fn read_message(recv: &mut RecvStream) -> Result<SyncMessage> {
    let mut len_buf = [0u8; 4];
    recv.read_exact(&mut len_buf).await?;
    let len = u32::from_le_bytes(len_buf);

    if len > MAX_MESSAGE_BYTES {
        return Err(anyhow::anyhow!(
            "Sync message too large: {} bytes (max {})",
            len,
            MAX_MESSAGE_BYTES
        ));
    }

    let mut buf = vec![0u8; len as usize];
    recv.read_exact(&mut buf).await?;
    let message = bincode::deserialize(&buf)?;
    Ok(message)
}
