//! SyncNode: top-level iroh networking component.
//!
//! Creates an iroh Endpoint from an ed25519 identity key, registers
//! the sync protocol ALPN, and provides methods for vault-scoped
//! gossip topics and QUIC bi-stream sync sessions.

use anyhow::{Context, Result};
use ed25519_dalek::SigningKey;
use iroh::{Endpoint, EndpointId, RelayMode, SecretKey};
use iroh::endpoint::presets;
use iroh::protocol::Router;
use iroh_gossip::{Gossip, TopicId};
use iroh_gossip::net::GOSSIP_ALPN;
use tracing::info;

use crate::peer_id::VaultId;
use crate::network::streams::{InboundSyncRx, SyncStreamHandler};

/// ALPN identifier for our custom sync protocol.
///
/// iroh's Router dispatches incoming QUIC connections to the correct handler
/// based on ALPN. This ALPN routes to our QUIC stream sync handler.
pub const SYNC_ALPN: &[u8] = b"obsidian-memory/sync/1";

/// The iroh-based sync node.
///
/// Owns the QUIC endpoint, gossip instance, and protocol router.
/// One `SyncNode` per daemon/plugin instance — not per vault.
/// Vaults get per-topic gossip subscriptions via `join_vault_gossip()`.
pub struct SyncNode {
    /// The underlying QUIC endpoint.
    pub endpoint: Endpoint,
    /// The gossip subsystem (HyParView + PlumTree).
    pub gossip: Gossip,
    /// Inbound sync requests from remote peers.
    ///
    /// Drive this in a task to process incoming sync requests. Each item
    /// carries a `SyncMessage` and a one-shot channel to send the response.
    pub inbound_sync_rx: InboundSyncRx,
    /// The protocol router (dispatches by ALPN).
    router: Router,
}

impl SyncNode {
    /// Construct a `SyncNode` from pre-built components.
    ///
    /// Intended for tests that need to control exactly how the endpoint,
    /// gossip, and router are configured (e.g., relay-disabled local testing).
    /// Normal callers should use [`SyncNode::new`] instead.
    pub fn new_from_parts(
        endpoint: Endpoint,
        gossip: Gossip,
        inbound_sync_rx: InboundSyncRx,
        router: Router,
    ) -> Self {
        Self {
            endpoint,
            gossip,
            inbound_sync_rx,
            router,
        }
    }

    /// Create a new SyncNode from an ed25519 secret key.
    ///
    /// The public key becomes this node's `EndpointId` (and derives our `PeerId`).
    /// The endpoint binds to a random available port.
    pub async fn new(secret_key_bytes: [u8; 32]) -> Result<Self> {
        let signing_key = SigningKey::from_bytes(&secret_key_bytes);
        let secret_key = SecretKey::from_bytes(&signing_key.to_bytes());

        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(secret_key)
            .relay_mode(RelayMode::Default)
            .bind()
            .await
            .context("Failed to create iroh endpoint")?;

        info!(node_id = %endpoint.id(), "Iroh endpoint created");

        let gossip = Gossip::builder().spawn(endpoint.clone());
        let (sync_handler, inbound_sync_rx) = SyncStreamHandler::new();

        let router = Router::builder(endpoint.clone())
            .accept(GOSSIP_ALPN, gossip.clone())
            .accept(SYNC_ALPN.to_vec(), sync_handler)
            .spawn();

        Ok(Self {
            endpoint,
            gossip,
            inbound_sync_rx,
            router,
        })
    }

    /// Create a SyncNode for testing with relay disabled.
    ///
    /// Both endpoints must be on the same machine. Provide each other's
    /// `EndpointAddr` via a `MemoryLookup` so they can dial directly.
    #[cfg(test)]
    pub async fn new_for_test(secret_key_bytes: [u8; 32]) -> Result<Self> {
        use iroh::address_lookup::memory::MemoryLookup;

        let signing_key = SigningKey::from_bytes(&secret_key_bytes);
        let secret_key = SecretKey::from_bytes(&signing_key.to_bytes());

        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(secret_key)
            .relay_mode(RelayMode::Disabled)
            .bind()
            .await
            .context("Failed to create iroh test endpoint")?;

        let memory_lookup = MemoryLookup::new();
        endpoint.address_lookup()?.add(memory_lookup);

        let gossip = Gossip::builder().spawn(endpoint.clone());
        let (sync_handler, inbound_sync_rx) = SyncStreamHandler::new();

        let router = Router::builder(endpoint.clone())
            .accept(GOSSIP_ALPN, gossip.clone())
            .accept(SYNC_ALPN.to_vec(), sync_handler)
            .spawn();

        Ok(Self {
            endpoint,
            gossip,
            inbound_sync_rx,
            router,
        })
    }

    /// This node's iroh EndpointId (matches our ed25519 public key).
    pub fn node_id(&self) -> EndpointId {
        self.endpoint.id()
    }

    /// Derive a deterministic gossip TopicId from a VaultId.
    ///
    /// Each vault gets its own gossip topic, scoped to peers who share that vault.
    /// The VaultId's `u64` value is written into the first 8 bytes of a 32-byte
    /// little-endian array to produce a consistent TopicId.
    pub fn vault_topic(vault_id: &VaultId) -> TopicId {
        let mut bytes = [0u8; 32];
        bytes[..8].copy_from_slice(&vault_id.as_u64().to_le_bytes());
        TopicId::from_bytes(bytes)
    }

    /// Subscribe to gossip for a specific vault.
    ///
    /// `bootstrap_nodes` should be the `EndpointId`s of known peers for that vault.
    /// At least one bootstrap node is needed to join the gossip swarm.
    pub async fn join_vault_gossip(
        &self,
        vault_id: &VaultId,
        bootstrap_nodes: Vec<EndpointId>,
    ) -> Result<crate::network::gossip::VaultGossip> {
        let topic = Self::vault_topic(vault_id);
        let handle = self
            .gossip
            .subscribe(topic, bootstrap_nodes)
            .await
            .context("Failed to subscribe to vault gossip topic")?;

        Ok(crate::network::gossip::VaultGossip::new(handle, topic))
    }

    /// Shut down the node, closing all connections.
    pub async fn shutdown(self) -> Result<()> {
        self.router.shutdown().await?;
        self.endpoint.close().await;
        Ok(())
    }
}
