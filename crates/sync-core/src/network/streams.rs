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
use p2p_core::streams::{read_length_prefixed, write_length_prefixed};
use p2p_core::{AcceptError, Connection, P2pNode, PeerAddr, ProtocolHandler};
use tokio::sync::{mpsc, oneshot};
use tracing::{debug, warn};

use crate::peer_id::PeerId;

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
    /// The remote peer's identity, extracted from the QUIC connection's TLS certificate.
    pub remote_id: PeerId,
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
        let remote_endpoint_id = connection.remote_id();
        let remote_id = PeerId::from_bytes(*remote_endpoint_id.as_bytes());

        let (mut send, mut recv) = connection
            .accept_bi()
            .await
            .map_err(AcceptError::from_err)?;

        let message_bytes = read_length_prefixed(&mut recv)
            .await
            .map_err(|e| AcceptError::from_boxed(e.into()))?;
        debug!(peer = %remote_endpoint_id, "Inbound sync message: {} bytes", message_bytes.len());

        let (reply_tx, reply_rx) = oneshot::channel();
        let request = InboundSyncRequest {
            message_bytes,
            reply_tx,
            remote_id,
        };

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
            _ = tokio::time::sleep(crate::time_scale::scaled(std::time::Duration::from_secs(30))) => {
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
/// Dials through `P2pNode::connect` (Tier-1, on `SYNC_ALPN`) and frames over the
/// returned connection. Used by the integration tests' single-round-trip sync
/// path; the daemon's production path is its own pumped handler (`sync_stream.rs`).
pub async fn connect_and_sync_raw(
    node: &P2pNode,
    peer: PeerAddr,
    request_bytes: &[u8],
) -> Result<Vec<u8>> {
    let alpn = crate::network::SYNC_ALPN;
    let connection = node.connect(&peer, alpn).await?;
    let (mut send, mut recv) = connection.open_bi().await?;

    write_length_prefixed(&mut send, request_bytes).await?;
    send.finish()?;

    read_length_prefixed(&mut recv).await
}
