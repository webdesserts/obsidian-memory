//! QUIC bi-stream sync protocol handler.
//!
//! `SyncStreamHandler` implements iroh's `ProtocolHandler` trait, which means
//! the Router dispatches incoming connections on `SYNC_ALPN` directly to it.
//!
//! # Protocol flow
//!
//! **Inbound (peer opens a connection to us):**
//! 1. Peer opens a QUIC connection with `SYNC_ALPN`.
//! 2. Peer opens a bi-directional stream and sends raw length-prefixed bytes.
//! 3. We forward the raw bytes to the event channel for the vault layer to process.
//! 4. The vault layer writes the response back (also raw bytes) via the provided one-shot channel.
//! 5. We write the raw response bytes to the stream.
//!
//! **Outbound (we initiate sync with a peer on `NeighborUp`):**
//! Call [`connect_and_sync_raw`] to open a bi-stream to the peer, send raw
//! request bytes, and receive raw response bytes. The vault layer handles
//! serialization on both ends so no extra encode/decode step is needed here.
//!
//! This handler intentionally does *not* contain vault logic. It is a thin
//! transport layer that frames messages and passes raw bytes to the caller
//! via an async channel.

use std::sync::Arc;

use anyhow::Result;
use iroh::endpoint::{Connection, RecvStream, SendStream};
use iroh::protocol::{AcceptError, ProtocolHandler};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

/// Maximum byte length for a framed sync message.
///
/// 64 MiB covers a realistic worst case of syncing a large vault in one batch.
/// Messages larger than this are rejected to prevent memory exhaustion.
const MAX_MESSAGE_BYTES: u32 = 64 * 1024 * 1024;

/// An inbound sync request received from a remote peer.
///
/// Raw bincode bytes are forwarded directly to the vault layer so no
/// extra deserialize/serialize round-trip is needed inside this handler.
/// The caller passes the bytes to `vault.process_sync_message` and sends the
/// resulting response bytes back through `reply_tx`.
///
/// The handler blocks the QUIC stream until the response arrives or
/// `reply_tx` is dropped (which closes the stream without a response).
pub struct InboundSyncRequest {
    /// Raw bincode-encoded `SyncMessage` bytes received from the remote peer.
    pub message_bytes: Vec<u8>,
    /// Send the raw response bytes back through here.
    pub reply_tx: oneshot::Sender<Vec<u8>>,
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
    /// Reads a length-prefixed message as raw bytes, forwards them to the
    /// inbound channel, then writes the raw response bytes back to the stream.
    async fn accept(&self, connection: Connection) -> Result<(), AcceptError> {
        let (mut send, mut recv) = connection
            .accept_bi()
            .await
            .map_err(AcceptError::from_err)?;

        let message_bytes = read_length_prefixed(&mut recv)
            .await
            .map_err(|e| AcceptError::from_boxed(e.into()))?;
        debug!("Inbound sync message: {} bytes", message_bytes.len());

        let (reply_tx, reply_rx) = oneshot::channel();
        let request = InboundSyncRequest { message_bytes, reply_tx };

        if self.inbound_tx.send(request).is_err() {
            // No one is listening for inbound requests — close the stream.
            warn!("Inbound sync request dropped: no receiver");
            return Ok(());
        }

        // Wait for the vault layer to respond.
        match reply_rx.await {
            Ok(response_bytes) => {
                write_length_prefixed(&mut send, &response_bytes)
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
        // reading the response we just sent. Cap at 30 seconds to avoid hanging
        // on a peer that crashes after receiving its response.
        tokio::select! {
            _ = connection.closed() => {}
            _ = tokio::time::sleep(std::time::Duration::from_secs(30)) => {
                debug!("Closing idle inbound connection after 30s timeout");
                connection.close(0u32.into(), b"timeout");
            }
        }

        Ok(())
    }
}

/// Open a QUIC bi-stream to `peer`, send raw request bytes, and return raw response bytes.
///
/// Skips the serialize/deserialize step — the vault layer already produces and
/// consumes bincode bytes, so passing raw bytes avoids a redundant round-trip.
///
/// Used when initiating sync after a `NeighborUp` or change-received gossip event.
pub async fn connect_and_sync_raw(
    endpoint: &iroh::Endpoint,
    peer: impl Into<iroh::EndpointAddr>,
    request_bytes: &[u8],
) -> Result<Vec<u8>> {
    let alpn = super::node::SYNC_ALPN;
    let connection = endpoint.connect(peer, alpn).await?;
    let (mut send, mut recv) = connection.open_bi().await?;

    write_length_prefixed(&mut send, request_bytes).await?;
    send.finish()?;

    read_length_prefixed(&mut recv).await
}

/// Write a length-prefixed byte slice to the stream.
///
/// Format: `[u32 little-endian length][bytes]`
pub(super) async fn write_length_prefixed(send: &mut SendStream, bytes: &[u8]) -> Result<()> {
    let len = u32::try_from(bytes.len())
        .map_err(|_| anyhow::anyhow!("Message too large to frame: {} bytes", bytes.len()))?;
    send.write_all(&len.to_le_bytes()).await?;
    send.write_all(bytes).await?;
    Ok(())
}

/// Read a length-prefixed byte slice from the stream.
///
/// Format: `[u32 little-endian length][bytes]`
pub(super) async fn read_length_prefixed(recv: &mut RecvStream) -> Result<Vec<u8>> {
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
    Ok(buf)
}
