//! SyncNode: top-level iroh networking component.
//!
//! Creates an iroh Endpoint from an ed25519 identity key, registers
//! the sync protocol ALPN, and provides methods for vault-scoped
//! gossip topics and QUIC bi-stream sync sessions.

use anyhow::{Context, Result};
use ed25519_dalek::SigningKey;
use iroh::{Endpoint, EndpointId, RelayMap, RelayMode, RelayUrl, SecretKey};
use iroh::endpoint::presets;
use iroh::protocol::Router;
use iroh_gossip::{Gossip, TopicId};
use iroh_gossip::net::GOSSIP_ALPN;
use tracing::info;

use crate::peer_id::VaultId;
#[cfg(feature = "native")]
use crate::network::pairing::{PairingEvent, PairingStreamHandler, PAIRING_ALPN};
use crate::network::streams::{InboundSyncRx, SyncStreamHandler};

/// ALPN identifier for our custom sync protocol.
///
/// iroh's Router dispatches incoming QUIC connections to the correct handler
/// based on ALPN. This ALPN routes to our QUIC stream sync handler.
pub const SYNC_ALPN: &[u8] = b"obsidian-memory/sync/1";

/// The mDNS service name for obsidian-sync mesh discovery.
///
/// Peers using the same service name can discover each other on the LAN.
/// Peers on a different service name are invisible to each other.
#[cfg(feature = "native")]
const MDNS_SERVICE_NAME: &str = "obsidian-sync";

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
    /// Inbound pairing events from new devices (native only).
    ///
    /// Drive this in the daemon event loop to process pairing requests.
    #[cfg(feature = "native")]
    pub inbound_pairing_rx: tokio::sync::mpsc::UnboundedReceiver<PairingEvent>,
    /// The protocol router (dispatches by ALPN).
    router: Router,
    /// mDNS address lookup for LAN mesh discovery (native only).
    #[cfg(feature = "native")]
    mdns: Option<iroh::address_lookup::MdnsAddressLookup>,
}

impl SyncNode {
    /// Construct a `SyncNode` from pre-built components.
    ///
    /// Intended for tests that need to control exactly how the endpoint,
    /// gossip, and router are configured (e.g., relay-disabled local testing).
    /// Normal callers should use [`SyncNode::new`] instead.
    ///
    /// Pairing is disabled in this constructor — `inbound_pairing_rx` will
    /// never yield events. Use `new_from_parts_with_pairing` if you need it.
    pub fn new_from_parts(
        endpoint: Endpoint,
        gossip: Gossip,
        inbound_sync_rx: InboundSyncRx,
        router: Router,
    ) -> Self {
        // Provide a dummy pairing channel that never yields (native only).
        #[cfg(feature = "native")]
        let (_tx, inbound_pairing_rx) = tokio::sync::mpsc::unbounded_channel::<PairingEvent>();
        Self {
            endpoint,
            gossip,
            inbound_sync_rx,
            #[cfg(feature = "native")]
            inbound_pairing_rx,
            router,
            #[cfg(feature = "native")]
            mdns: None,
        }
    }

    /// Create a new SyncNode from an ed25519 secret key.
    ///
    /// The public key becomes this node's `EndpointId` (and derives our `PeerId`).
    /// The endpoint binds to a random available port.
    ///
    /// `extra_relay` — if provided, the endpoint uses a custom `RelayMap` containing
    /// only this relay URL instead of the default number 0 relay servers. Pass this
    /// when the daemon is running its own embedded relay so peers can route through it.
    pub async fn new(secret_key_bytes: [u8; 32], extra_relay: Option<&RelayUrl>) -> Result<Self> {
        let signing_key = SigningKey::from_bytes(&secret_key_bytes);
        let secret_key = SecretKey::from_bytes(&signing_key.to_bytes());

        let relay_mode = match extra_relay {
            Some(url) => RelayMode::Custom(RelayMap::from_iter([url.clone()])),
            None => RelayMode::Default,
        };

        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(secret_key)
            .relay_mode(relay_mode)
            .bind()
            .await
            .context("Failed to create iroh endpoint")?;

        info!(node_id = %endpoint.id(), "Iroh endpoint created");

        #[cfg(feature = "native")]
        let mdns = {
            use iroh::address_lookup::MdnsAddressLookup;
            match MdnsAddressLookup::builder()
                .service_name(MDNS_SERVICE_NAME)
                .build(endpoint.id())
            {
                Ok(mdns) => {
                    if let Ok(lookup) = endpoint.address_lookup() {
                        lookup.add(mdns.clone());
                        info!("mDNS mesh discovery enabled");
                    }
                    Some(mdns)
                }
                Err(e) => {
                    tracing::warn!("Failed to create mDNS address lookup, mesh discovery disabled: {e}");
                    None
                }
            }
        };

        let gossip = Gossip::builder().spawn(endpoint.clone());
        let (sync_handler, inbound_sync_rx) = SyncStreamHandler::new();

        #[cfg(feature = "native")]
        let (pairing_handler, inbound_pairing_rx) = PairingStreamHandler::new();

        let router = {
            let builder = Router::builder(endpoint.clone())
                .accept(GOSSIP_ALPN, gossip.clone())
                .accept(SYNC_ALPN.to_vec(), sync_handler);
            #[cfg(feature = "native")]
            let builder = builder.accept(PAIRING_ALPN.to_vec(), pairing_handler);
            builder.spawn()
        };

        Ok(Self {
            endpoint,
            gossip,
            inbound_sync_rx,
            #[cfg(feature = "native")]
            inbound_pairing_rx,
            router,
            #[cfg(feature = "native")]
            mdns,
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

        #[cfg(feature = "native")]
        let (pairing_handler, inbound_pairing_rx) = PairingStreamHandler::new();

        let router = {
            let builder = Router::builder(endpoint.clone())
                .accept(GOSSIP_ALPN, gossip.clone())
                .accept(SYNC_ALPN.to_vec(), sync_handler);
            #[cfg(feature = "native")]
            let builder = builder.accept(PAIRING_ALPN.to_vec(), pairing_handler);
            builder.spawn()
        };

        Ok(Self {
            endpoint,
            gossip,
            inbound_sync_rx,
            #[cfg(feature = "native")]
            inbound_pairing_rx,
            router,
            #[cfg(feature = "native")]
            mdns: None,
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

    /// Publish this node's mesh metadata via mDNS so LAN peers can discover it.
    ///
    /// Other devices running obsidian-sync on the same LAN will see this
    /// node's `MeshMetadata` in their `subscribe_discovery()` stream.
    /// Devices with the same `VaultId` belong to the same mesh.
    ///
    /// Relay URL is included when provided so discovered peers can connect
    /// even without direct address information.
    #[cfg(feature = "native")]
    pub fn publish_mesh_info(
        &self,
        metadata: &crate::network::discovery::MeshMetadata,
        relay_url: Option<&RelayUrl>,
    ) {
        use iroh::address_lookup::{AddressLookup, EndpointData};

        let Some(ref mdns) = self.mdns else {
            tracing::debug!("mDNS not available, skipping mesh info publish");
            return;
        };

        let json = match serde_json::to_string(metadata) {
            Ok(s) => s,
            Err(e) => {
                tracing::warn!("Failed to serialize MeshMetadata for mDNS: {e}");
                return;
            }
        };

        let user_data = match json.parse::<iroh::address_lookup::UserData>() {
            Ok(ud) => ud,
            Err(e) => {
                tracing::warn!("MeshMetadata JSON too long for mDNS UserData ({} bytes): {e}", json.len());
                return;
            }
        };

        let data = EndpointData::new([])
            .with_user_data(Some(user_data))
            .with_relay_url(relay_url.cloned());

        mdns.publish(&data);
        info!(mesh = %metadata.mesh, vid = %metadata.vid, "Published mesh info via mDNS");
    }

    /// Subscribe to mDNS discovery events for nearby meshes.
    ///
    /// Each `DiscoveryEvent::Discovered` event carries a peer's `EndpointInfo`,
    /// including their `UserData` which contains JSON-encoded `MeshMetadata`.
    /// Parse the `UserData` to extract the mesh name and VaultId, then group
    /// peers by VaultId to form `DiscoveredMesh` entries.
    #[cfg(feature = "native")]
    pub async fn subscribe_discovery(
        &self,
    ) -> Option<impl n0_future::Stream<Item = iroh::address_lookup::DiscoveryEvent> + Unpin + use<>> {
        let mdns = self.mdns.as_ref()?;
        Some(mdns.subscribe().await)
    }

    /// Shut down the node, closing all connections.
    pub async fn shutdown(self) -> Result<()> {
        self.router.shutdown().await?;
        self.endpoint.close().await;
        Ok(())
    }
}
