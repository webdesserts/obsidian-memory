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
use p2p_core::PeerId;
use rand::Rng;
use tokio::sync::mpsc;
use tracing::{debug, warn};

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
    /// Opaque discriminator that makes successive notifications for the same path
    /// serialize to distinct bytes, so iroh-gossip's content-hash MessageId dedup
    /// cannot suppress a same-path follow-up within its seen-cache window. The
    /// receiver MUST treat this as opaque — it carries no ordering or identity
    /// meaning, it exists solely to defeat content-based dedup (Issue 2).
    pub nonce: u64,
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
    /// The sender's full membership roster (live peers + tombstones) — receiving
    /// peers merge it by union with tombstone-precedence to converge on one
    /// allowlist. Sent on reconnect/reconcile so trust propagates even to peers
    /// that were offline at a pairing's single-shot `AllowlistUpdate` broadcast.
    AllowlistRoster(Vec<AllowedPeer>),
}

/// Events produced by the gossip subscription loop.
#[derive(Debug)]
pub enum GossipEvent {
    /// A peer joined the gossip swarm for this vault.
    NeighborUp(PeerId),
    /// A peer left the gossip swarm for this vault.
    NeighborDown(PeerId),
    /// A change notification broadcast received from a peer.
    ChangeReceived {
        from: PeerId,
        notification: ChangeNotification,
    },
    /// A peer was added to the allowlist by a trusted mesh member.
    AllowlistUpdate { from: PeerId, peer: AllowedPeer },
    /// A full membership roster received from a trusted mesh member, to be
    /// merged into the local allowlist by union with tombstone-precedence.
    AllowlistRoster {
        from: PeerId,
        peers: Vec<AllowedPeer>,
    },
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
    /// Random session base for change-notification nonces, drawn once per
    /// `VaultGossip`. XORed with `notif_seq` to discriminate successive
    /// same-path notifications (see `ChangeNotification.nonce`). A daemon
    /// restart builds a new `VaultGossip` with a fresh salt, so the counter
    /// resetting to 0 cannot collide with pre-restart nonces.
    notif_salt: u64,
    /// Monotonic per-broadcast counter, 0 at construction. Makes within-session
    /// nonce uniqueness deterministic (XOR with a fixed salt is a bijection).
    notif_seq: u64,
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
                        // Diagnostic only: the daemon emits the enriched INFO
                        // connect/disconnect notice off the PeerRegistry liveness
                        // transition. This is the raw HyParView membership edge.
                        debug!(peer = %node_id, "Gossip NeighborUp");
                        // iroh's EndpointId is the same 32 bytes as our PeerId; this
                        // reverse conversion is infallible.
                        GossipEvent::NeighborUp(PeerId::from_bytes(*node_id.as_bytes()))
                    }
                    Event::NeighborDown(node_id) => {
                        debug!(peer = %node_id, "Gossip NeighborDown");
                        GossipEvent::NeighborDown(PeerId::from_bytes(*node_id.as_bytes()))
                    }
                    Event::Received(msg) => {
                        let from = PeerId::from_bytes(*msg.delivered_from.as_bytes());
                        match bincode::deserialize::<GossipMessage>(&msg.content) {
                            Ok(GossipMessage::ChangeNotification(notification)) => {
                                debug!(path = %notification.path, from = %msg.delivered_from, "Change notification received");
                                GossipEvent::ChangeReceived { from, notification }
                            }
                            Ok(GossipMessage::AllowlistUpdate(peer)) => {
                                debug!(peer_id = %peer.node_id, from = %msg.delivered_from, "Allowlist update received");
                                GossipEvent::AllowlistUpdate { from, peer }
                            }
                            Ok(GossipMessage::AllowlistRoster(peers)) => {
                                debug!(count = peers.len(), from = %msg.delivered_from, "Allowlist roster received");
                                GossipEvent::AllowlistRoster { from, peers }
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
            notif_salt: rand::rng().random(),
            notif_seq: 0,
        }
    }

    /// Broadcast a change notification to all vault peers.
    ///
    /// The notification is small (path only). Peers who receive it will
    /// open a QUIC stream to pull the actual document updates.
    pub async fn broadcast_change(&mut self, path: &str) -> Result<()> {
        // The nonce makes successive same-path notifications serialize to distinct
        // bytes so iroh-gossip's content-hash dedup can't suppress a follow-up (a
        // create-notif followed by a same-path delete-notif within the seen window).
        // `wrapping_add` so a pathological 2^64-broadcast session can't panic in debug.
        let nonce = self.notif_salt ^ self.notif_seq;
        self.notif_seq = self.notif_seq.wrapping_add(1);
        let msg = GossipMessage::ChangeNotification(ChangeNotification {
            path: path.to_string(),
            nonce,
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

    /// Broadcast the full membership roster to all vault peers.
    ///
    /// Used by the reconnect/reconcile convergence path so a peer that missed a
    /// pairing's single-shot `AllowlistUpdate` still learns the whole roster.
    /// Goes through `broadcast_message`, so it inherits the 1KB size cap — the
    /// caller is responsible for the pre-check and per-peer-delta fallback when a
    /// large roster would exceed it (see the daemon's `push_allowlist_roster`).
    pub async fn broadcast_allowlist_roster(&mut self, peers: &[AllowedPeer]) -> Result<()> {
        let msg = GossipMessage::AllowlistRoster(peers.to_vec());
        self.broadcast_message(&msg).await
    }

    /// Re-bootstrap gossip toward a set of known peers without re-subscribing.
    ///
    /// Sends `Command::JoinPeers` to the gossip actor on the already-subscribed
    /// topic, telling HyParView to (re-)dial those peers. Used by the reconnect
    /// supervisor to recover from a partition: after a `NeighborDown`, neither
    /// side dials the other again on its own, so the supervisor re-seeds relay
    /// hints and calls this to re-establish the swarm. Re-subscribing would churn
    /// the swarm; `join_peers` is the targeted re-bootstrap.
    pub async fn rejoin_peers(&self, peers: Vec<PeerId>) -> Result<()> {
        self.sender
            .join_peers(peer_ids_to_endpoints(peers))
            .await
            .map_err(|e| anyhow::anyhow!("Failed to re-join gossip peers: {e}"))
    }

    /// A cloneable handle that can re-join peers from a spawned task.
    ///
    /// `VaultGossip` itself is not `Clone` (it owns the event receiver and the
    /// pump task), but the reconnect supervisor's bootstrap-connect runs in a
    /// spawned task that must re-queue a Join just before adopting the connection.
    /// This hands out a lightweight clone of the underlying gossip sender for that
    /// purpose without exposing the receiver.
    pub fn rejoin_handle(&self) -> GossipRejoinHandle {
        GossipRejoinHandle {
            sender: self.sender.clone(),
        }
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

/// A cloneable handle for re-joining gossip peers from a spawned task.
///
/// Obtained via [`VaultGossip::rejoin_handle`]. Carries only a clone of the
/// gossip sender, so it can be moved into a `tokio::spawn`ed task (the reconnect
/// supervisor's bootstrap-connect) where the non-`Clone` `VaultGossip` cannot go.
#[derive(Clone)]
pub struct GossipRejoinHandle {
    sender: GossipSender,
}

impl GossipRejoinHandle {
    /// Re-join the given peers — see [`VaultGossip::rejoin_peers`].
    pub async fn rejoin_peers(&self, peers: Vec<PeerId>) -> Result<()> {
        self.sender
            .join_peers(peer_ids_to_endpoints(peers))
            .await
            .map_err(|e| anyhow::anyhow!("Failed to re-join gossip peers: {e}"))
    }
}

/// Convert `PeerId`s to iroh `EndpointId`s for a gossip re-join, dropping any
/// legacy/non-curve-point id (it could never be a reachable bootstrap target).
///
/// gossip.rs still speaks iroh's gossip transport directly (the transport descent
/// is Stage C), so the `PeerId → EndpointId` conversion is inlined here rather than
/// going through p2p-core's crate-private adapter.
fn peer_ids_to_endpoints(peers: Vec<PeerId>) -> Vec<EndpointId> {
    peers
        .into_iter()
        .filter_map(|p| EndpointId::from_bytes(p.as_bytes()).ok())
        .collect()
}
