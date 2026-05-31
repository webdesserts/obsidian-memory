//! Vault-scoped gossip topic management.
//!
//! Each vault subscribes to its own iroh-gossip topic (TopicId derived from VaultId).
//! The gossip layer handles:
//! - Membership via HyParView (`NeighborUp`/`NeighborDown` events)
//! - Lightweight broadcast via PlumTree (file change notifications, allowlist updates)
//!
//! All gossip payloads are wrapped in [`GossipMessage`] so the wire format can
//! carry multiple message types without breaking framing. This is a breaking
//! wire format change introduced on `v0.5.x`.
//!
//! Gossip messages must be small (~1KB). Large sync data goes through QUIC streams.

use anyhow::Result;
use bytes::Bytes;
use futures::StreamExt;
use iroh::EndpointId;
use iroh_gossip::{
    TopicId,
    api::{Event, GossipSender},
};
use n0_future::task;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::allowlist::AllowedPeer;

/// Maximum size of a gossip message payload in bytes.
///
/// PlumTree's eager push has practical limits around 1KB. Larger messages
/// should be sent through QUIC bi-streams instead.
pub const MAX_GOSSIP_MESSAGE_SIZE: usize = 1024;

/// A notification broadcasted via gossip when a file changes.
///
/// Peers receiving this notification open a QUIC stream to pull updates.
/// Kept small (under 1KB) to stay within gossip size limits.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ChangeNotification {
    /// Path of the modified file
    pub path: String,
}

/// Envelope for all gossip messages.
///
/// Wrapping messages in an enum lets us add new gossip message types without
/// breaking the framing layer — deserialization errors from unknown or corrupt
/// messages are logged and skipped rather than causing a protocol failure.
///
/// NOTE: This is a breaking wire format change relative to pre-v0.5.x versions,
/// which serialized `ChangeNotification` directly. All peers must be upgraded
/// together.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub enum GossipMessage {
    /// A file changed — receiving peers should pull the update via QUIC.
    ChangeNotification(ChangeNotification),
    /// A peer was added to the allowlist — receiving peers should add them locally.
    AllowlistUpdate(AllowedPeer),
}

/// Events produced by the gossip subscription loop.
#[derive(Debug)]
pub enum GossipEvent {
    /// A peer joined the gossip swarm for this vault.
    NeighborUp(EndpointId),
    /// A peer left the gossip swarm for this vault.
    NeighborDown(EndpointId),
    /// A change notification broadcast received from a peer.
    ChangeReceived {
        from: EndpointId,
        notification: ChangeNotification,
    },
    /// A peer was added to the allowlist by a trusted mesh member.
    AllowlistUpdate { from: EndpointId, peer: AllowedPeer },
}

/// Handle for a vault-scoped gossip subscription.
///
/// Provides broadcast capability (via the sender) and event stream (via the task).
pub struct VaultGossip {
    /// Channel to broadcast messages to the gossip swarm.
    sender: GossipSender,
    /// The gossip topic ID.
    pub topic: TopicId,
    /// Events from the gossip subscription, pumped by a background task.
    pub event_rx: mpsc::UnboundedReceiver<GossipEvent>,
    /// Background task handle (kept alive while subscription is active).
    _task: task::JoinHandle<()>,
}

impl VaultGossip {
    /// Create a new VaultGossip from an iroh subscription handle.
    pub fn new(handle: iroh_gossip::api::GossipTopic, topic: TopicId) -> Self {
        let (sender, mut receiver) = handle.split();
        let (event_tx, event_rx) = mpsc::unbounded_channel();

        let task = task::spawn(async move {
            while let Some(result) = receiver.next().await {
                let event = match result {
                    Ok(e) => e,
                    Err(e) => {
                        debug!("Gossip stream error: {}", e);
                        break;
                    }
                };

                let gossip_event = match event {
                    Event::NeighborUp(node_id) => {
                        info!(peer = %node_id, "Gossip NeighborUp");
                        GossipEvent::NeighborUp(node_id)
                    }
                    Event::NeighborDown(node_id) => {
                        info!(peer = %node_id, "Gossip NeighborDown");
                        GossipEvent::NeighborDown(node_id)
                    }
                    Event::Received(msg) => {
                        match bincode::deserialize::<GossipMessage>(&msg.content) {
                            Ok(GossipMessage::ChangeNotification(notification)) => {
                                debug!(path = %notification.path, from = %msg.delivered_from, "Change notification received");
                                GossipEvent::ChangeReceived {
                                    from: msg.delivered_from,
                                    notification,
                                }
                            }
                            Ok(GossipMessage::AllowlistUpdate(peer)) => {
                                debug!(peer_id = %peer.node_id, from = %msg.delivered_from, "Allowlist update received");
                                GossipEvent::AllowlistUpdate {
                                    from: msg.delivered_from,
                                    peer,
                                }
                            }
                            Err(e) => {
                                warn!(
                                    "Failed to deserialize gossip message from {}: {}",
                                    msg.delivered_from, e
                                );
                                continue;
                            }
                        }
                    }
                    Event::Lagged => {
                        debug!("Gossip lagged (message buffer overflow)");
                        continue;
                    }
                };

                if event_tx.send(gossip_event).is_err() {
                    break; // Receiver dropped
                }
            }
        });

        Self {
            sender,
            topic,
            event_rx,
            _task: task,
        }
    }

    /// Broadcast a change notification to all vault peers.
    ///
    /// The notification is small (path only). Peers who receive it will
    /// open a QUIC stream to pull the actual document updates.
    pub async fn broadcast_change(&mut self, path: &str) -> Result<()> {
        let msg = GossipMessage::ChangeNotification(ChangeNotification {
            path: path.to_string(),
        });
        self.broadcast_message(&msg).await
    }

    /// Broadcast an allowlist update to all vault peers.
    ///
    /// Called after a successful pairing to propagate the new peer to the rest
    /// of the mesh. Peers who receive this add the new peer to their local
    /// allowlist if the sender is trusted.
    pub async fn broadcast_allowlist_update(&mut self, peer: &AllowedPeer) -> Result<()> {
        let msg = GossipMessage::AllowlistUpdate(peer.clone());
        self.broadcast_message(&msg).await
    }

    /// Serialize and broadcast a [`GossipMessage`] envelope.
    async fn broadcast_message(&mut self, msg: &GossipMessage) -> Result<()> {
        let bytes: Bytes = bincode::serialize(msg)?.into();
        if bytes.len() > MAX_GOSSIP_MESSAGE_SIZE {
            return Err(anyhow::anyhow!(
                "Gossip message too large: {} bytes (max {})",
                bytes.len(),
                MAX_GOSSIP_MESSAGE_SIZE
            ));
        }
        self.sender.broadcast(bytes).await?;
        Ok(())
    }
}
