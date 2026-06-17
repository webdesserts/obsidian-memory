//! SyncNode: top-level iroh networking component.
//!
//! Creates an iroh Endpoint from an ed25519 identity key, registers
//! the sync protocol ALPN, and provides methods for vault-scoped
//! gossip topics and QUIC bi-stream sync sessions.

use anyhow::{Context, Result};
use ed25519_dalek::SigningKey;
use iroh::endpoint::presets;
use iroh::protocol::Router;
use iroh::{Endpoint, EndpointId, RelayMap, RelayMode, RelayUrl, SecretKey};
#[cfg(feature = "native")]
use iroh::{EndpointAddr, address_lookup::memory::MemoryLookup};
use iroh_gossip::net::GOSSIP_ALPN;
use iroh_gossip::{Gossip, TopicId};
use std::sync::Arc;
use tracing::{info, warn};

#[cfg(feature = "native")]
use crate::allowlist::AllowlistStorage;
#[cfg(feature = "native")]
use crate::network::pairing::{PAIRING_ALPN, PairingEvent, PairingStreamHandler};
use crate::network::streams::{InboundSyncRx, SyncStreamHandler};
use crate::peer_id::VaultId;

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

/// Wraps iroh-gossip's connection handler with allowlist enforcement.
///
/// Non-allowlisted peers are rejected before the gossip protocol runs.
/// This prevents unpaired devices from joining the gossip swarm and
/// observing which files are changing, even if they can reach the network.
///
/// The pairing ALPN is a separate handler and is NOT affected — pairing
/// must remain open to allow new devices to join.
#[cfg(feature = "native")]
#[derive(Debug)]
struct AllowlistGossipHandler<A: AllowlistStorage + std::fmt::Debug> {
    gossip: Gossip,
    allowlist: Arc<A>,
}

#[cfg(feature = "native")]
impl<A: AllowlistStorage + std::fmt::Debug + 'static> iroh::protocol::ProtocolHandler
    for AllowlistGossipHandler<A>
{
    async fn accept(
        &self,
        connection: iroh::endpoint::Connection,
    ) -> Result<(), iroh::protocol::AcceptError> {
        use crate::peer_id::PeerId;

        let remote_id = connection.remote_id();
        let peer_id = PeerId::from_bytes(*remote_id.as_bytes());

        let allowed = match self.allowlist.list_peers().await {
            Ok(peers) => peers.iter().any(|p| p.node_id == peer_id),
            Err(e) => {
                warn!(peer = %remote_id, "Failed to read allowlist for gossip accept, denying: {}", e);
                false
            }
        };

        if !allowed {
            warn!(peer = %remote_id, "Gossip connection rejected — peer not in allowlist");
            connection.close(iroh::endpoint::VarInt::from_u32(1), b"not in allowlist");
            return Ok(());
        }

        self.gossip
            .handle_connection(connection)
            .await
            .map_err(iroh::protocol::AcceptError::from_err)
    }
}

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
    mdns: Option<crate::network::mesh_mdns::MeshMdns>,
    /// In-memory address lookup seeded with known peer relay hints (native only).
    ///
    /// Populated at startup from `DaemonConfig.peer_relays` so gossip bootstrap
    /// can resolve off-LAN peers through their relay before mDNS finds them.
    /// Also updated at runtime after pairing via `add_peer_relay`.
    ///
    /// Exposed as `pub` so integration tests can inspect hint registration
    /// without a live connection — the canonical production verification path
    /// is full gossip connectivity (see relay_integration tests).
    #[cfg(feature = "native")]
    pub peer_lookup: MemoryLookup,
}

impl SyncNode {
    /// Create a new SyncNode from an ed25519 secret key.
    ///
    /// The public key becomes this node's `EndpointId` (and derives our `PeerId`).
    /// The endpoint binds to a random available port.
    ///
    /// `extra_relay` — if provided, the endpoint uses a custom `RelayMap` containing
    /// only this relay URL. If `None`, the endpoint uses direct QUIC only (no relay).
    /// Pass the daemon's embedded relay URL so peers can route through it.
    ///
    /// `allowlist` — on native builds, gossip connections are pre-screened using this
    /// allowlist before the gossip protocol runs. Non-allowlisted peers are rejected
    /// immediately. Pass `Arc::new(FileAllowlistStorage::new(&vault))` from the daemon.
    #[cfg(feature = "native")]
    pub async fn new<A: AllowlistStorage + std::fmt::Debug + 'static>(
        secret_key_bytes: [u8; 32],
        extra_relay: Option<&RelayUrl>,
        allowlist: Arc<A>,
    ) -> Result<Self> {
        Self::build(secret_key_bytes, extra_relay, allowlist, false).await
    }

    /// Create a SyncNode whose endpoint has **no IP transports** — relay-only.
    ///
    /// Identical to [`SyncNode::new`] except the iroh endpoint is built with
    /// `clear_ip_transports()`, so it cannot open any direct/loopback QUIC path
    /// and can only reach peers through a relay. This reproduces the off-LAN /
    /// behind-NAT condition where the relay is the sole route — the topology that
    /// in-process localhost tests otherwise can't force, because two loopback
    /// nodes would discover each other's direct addresses and bypass the relay.
    ///
    /// Test-only (behind the `test-util` feature); never used in production.
    #[cfg(all(feature = "native", feature = "test-util"))]
    pub async fn new_relay_only<A: AllowlistStorage + std::fmt::Debug + 'static>(
        secret_key_bytes: [u8; 32],
        extra_relay: Option<&RelayUrl>,
        allowlist: Arc<A>,
    ) -> Result<Self> {
        Self::build(secret_key_bytes, extra_relay, allowlist, true).await
    }

    /// Shared constructor body for [`SyncNode::new`] and (test-only)
    /// [`SyncNode::new_relay_only`].
    ///
    /// `relay_only` strips IP transports from the endpoint so the only routing
    /// path is a relay. The production callers always pass `false`; only the
    /// `test-util` relay-only constructor passes `true`.
    #[cfg(feature = "native")]
    async fn build<A: AllowlistStorage + std::fmt::Debug + 'static>(
        secret_key_bytes: [u8; 32],
        extra_relay: Option<&RelayUrl>,
        allowlist: Arc<A>,
        relay_only: bool,
    ) -> Result<Self> {
        let signing_key = SigningKey::from_bytes(&secret_key_bytes);
        let secret_key = SecretKey::from_bytes(&signing_key.to_bytes());

        let relay_mode = match extra_relay {
            Some(url) => RelayMode::Custom(RelayMap::from_iter([url.clone()])),
            None => RelayMode::Disabled,
        };

        let builder = Endpoint::builder(presets::Minimal)
            .secret_key(secret_key)
            .relay_mode(relay_mode);
        // Relay-only: drop every IP transport so no direct/loopback path exists —
        // the endpoint must route through a relay (the off-LAN/NAT condition).
        let builder = if relay_only {
            builder.clear_ip_transports()
        } else {
            builder
        };
        let endpoint = builder
            .bind()
            .await
            .context("Failed to create iroh endpoint")?;

        info!(node_id = %endpoint.id(), "Iroh endpoint created");

        let mdns = {
            use crate::network::mesh_mdns::{MeshMdns, socket_addrs_to_port_addrs};
            let bound = endpoint.bound_sockets();
            let (port, ips) = socket_addrs_to_port_addrs(&bound);
            // Only advertise IPv4 sockets (V4Only mDNS scope).
            let v4_ips: Vec<std::net::IpAddr> = ips
                .into_iter()
                .filter(|ip| ip.is_ipv4())
                .collect();
            let rt = tokio::runtime::Handle::current();
            match MeshMdns::new(endpoint.id(), MDNS_SERVICE_NAME, port, v4_ips, &rt) {
                Ok(mdns) => {
                    match endpoint.address_lookup() {
                        Ok(lookup) => {
                            lookup.add(mdns.clone());
                            info!("mDNS mesh discovery enabled (V4Only)");
                        }
                        Err(e) => {
                            warn!("Failed to register mDNS with address lookup: {e}");
                        }
                    }
                    Some(mdns)
                }
                Err(e) => {
                    warn!("Failed to create mDNS address lookup, mesh discovery disabled: {e}");
                    None
                }
            }
        };

        // Register the peer-relay MemoryLookup alongside mDNS so gossip can
        // resolve off-LAN peers through hints seeded from persisted peer_relays.
        // Registration failure is non-fatal — we warn and continue without hints,
        // mirroring the mDNS registration pattern above.
        let peer_lookup = MemoryLookup::with_provenance("peer_relays");
        match endpoint.address_lookup() {
            Ok(lookup) => {
                lookup.add(peer_lookup.clone());
                info!("Peer relay address lookup registered");
            }
            Err(e) => {
                warn!("Failed to register peer relay address lookup: {e}");
            }
        }

        let gossip = Gossip::builder().spawn(endpoint.clone());
        let (sync_handler, inbound_sync_rx) = SyncStreamHandler::new();

        let (pairing_handler, inbound_pairing_rx) = PairingStreamHandler::new();

        let gossip_handler = AllowlistGossipHandler {
            gossip: gossip.clone(),
            allowlist,
        };

        let router = Router::builder(endpoint.clone())
            .accept(GOSSIP_ALPN, gossip_handler)
            .accept(SYNC_ALPN.to_vec(), sync_handler)
            .accept(PAIRING_ALPN.to_vec(), pairing_handler)
            .spawn();

        Ok(Self {
            endpoint,
            gossip,
            inbound_sync_rx,
            inbound_pairing_rx,
            router,
            mdns,
            peer_lookup,
        })
    }

    /// Create a new SyncNode from an ed25519 secret key (WASM build).
    ///
    /// The public key becomes this node's `EndpointId` (and derives our `PeerId`).
    /// The endpoint binds to a random available port.
    ///
    /// `extra_relay` — if provided, the endpoint uses a custom `RelayMap` containing
    /// only this relay URL. If `None`, the endpoint uses direct QUIC only (no relay).
    /// The plugin should always pass the daemon's relay URL for reliable connectivity.
    #[cfg(not(feature = "native"))]
    pub async fn new(secret_key_bytes: [u8; 32], extra_relay: Option<&RelayUrl>) -> Result<Self> {
        let signing_key = SigningKey::from_bytes(&secret_key_bytes);
        let secret_key = SecretKey::from_bytes(&signing_key.to_bytes());

        let relay_mode = match extra_relay {
            Some(url) => RelayMode::Custom(RelayMap::from_iter([url.clone()])),
            None => RelayMode::Disabled,
        };

        let endpoint = Endpoint::builder(presets::Minimal)
            .secret_key(secret_key)
            .relay_mode(relay_mode)
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

    /// This node's iroh EndpointId (matches our ed25519 public key).
    pub fn node_id(&self) -> EndpointId {
        self.endpoint.id()
    }

    /// Seed a relay hint for a peer into the address-lookup service (native only).
    ///
    /// After this call, gossip bootstrap with the peer's bare `EndpointId` will
    /// resolve the hint and attempt a connection through their relay. This is
    /// called at startup for each entry in `DaemonConfig.peer_relays`, and at
    /// runtime after a successful pairing.
    ///
    /// If the relay is unreachable off-LAN, connection attempts through that hint
    /// will simply fail; there is no automatic fallback to mDNS off-LAN because
    /// mDNS does not operate across network boundaries.
    #[cfg(feature = "native")]
    pub fn add_peer_relay(&self, endpoint_id: EndpointId, relay_url: &RelayUrl) {
        if endpoint_id == self.node_id() {
            tracing::warn!(
                "add_peer_relay called with our own EndpointId — ignoring to prevent \
                 self-connect (iroh rejects self-directed relay paths)"
            );
            return;
        }
        let addr = EndpointAddr::new(endpoint_id).with_relay_url(relay_url.clone());
        self.peer_lookup.add_endpoint_info(addr);
    }

    /// Replace a peer's relay hint in the address-lookup service (native only).
    ///
    /// Unlike `add_peer_relay`, which unions the new relay with any existing
    /// addresses for the peer, this completely overwrites the prior entry. Used
    /// when refreshing a hint that may be stale: the reconnect supervisor re-seeds
    /// before re-bootstrapping, and learn-on-exchange replaces a moved peer's relay.
    /// A union would let a stale, dead relay URL linger alongside the fresh one and
    /// keep getting dialed; the overwrite guarantees only the latest hint remains.
    #[cfg(feature = "native")]
    pub fn set_peer_relay(&self, endpoint_id: EndpointId, relay_url: &RelayUrl) {
        if endpoint_id == self.node_id() {
            tracing::warn!(
                "set_peer_relay called with our own EndpointId — ignoring to prevent \
                 self-connect (iroh rejects self-directed relay paths)"
            );
            return;
        }
        let addr = EndpointAddr::new(endpoint_id).with_relay_url(relay_url.clone());
        self.peer_lookup.set_endpoint_info(addr);
    }

    /// Evict a peer's relay hint from the address-lookup service (native only).
    ///
    /// Removing the entry from `peer_lookup` (`MemoryLookup`) is the only lever
    /// that stops a dead hint from being re-dialed: merely declining to re-seed
    /// it is insufficient because iroh-gossip's HyParView maintenance ALSO
    /// re-resolves whatever sits in the lookup on its own ~60s cadence, which
    /// re-feeds iroh's relay actor. With the entry gone, no re-resolution source
    /// can revive the relay, and iroh reaps the idle `ActiveRelayActor` within
    /// ~60s — that is what actually quiets the "No route to host" warn loop.
    ///
    /// The reconnect supervisor calls this to throttle a stale hint, then
    /// re-adds it via `set_peer_relay` on a slow cadence so a genuinely off-LAN
    /// peer that returns is still reached. Removing an absent key is a harmless
    /// no-op.
    #[cfg(feature = "native")]
    pub fn remove_peer_relay(&self, endpoint_id: EndpointId) {
        if endpoint_id == self.node_id() {
            tracing::warn!(
                "remove_peer_relay called with our own EndpointId — ignoring for \
                 consistency with add/set_peer_relay (we never seed ourselves)"
            );
            return;
        }
        self.peer_lookup.remove_endpoint_info(endpoint_id);
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

    /// Recover the `VaultId` encoded in a gossip topic's bytes.
    ///
    /// Inverse of [`vault_topic`](Self::vault_topic): reads the little-endian
    /// `u64` back out of `topic[0..8]`. Used when a pairing initiator must adopt
    /// the mesh's VaultId carried in `PairingResult::vault_topic` — the topic is
    /// the only place the VaultId travels over the wire, so this is the single
    /// defined inverse mapping rather than duplicating the id in a separate field.
    pub fn vault_id_from_topic(topic: &[u8; 32]) -> VaultId {
        VaultId::from(u64::from_le_bytes(topic[..8].try_into().unwrap()))
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
        use crate::network::mesh_mdns::socket_addrs_to_port_addrs;

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

        // Validate and send as user-data TXT attribute (routed through actor for ordering).
        match json.parse::<iroh::address_lookup::UserData>() {
            Ok(_) => mdns.set_user_data(&json),
            Err(e) => {
                tracing::warn!(
                    "MeshMetadata JSON too long for mDNS UserData ({} bytes): {e}",
                    json.len()
                );
                return;
            }
        }

        // Re-advertise current bound addresses (IPv4 only, matching V4Only scope).
        let bound = self.endpoint.bound_sockets();
        let v4_bound: Vec<std::net::SocketAddr> =
            bound.into_iter().filter(|sa| sa.is_ipv4()).collect();
        if !v4_bound.is_empty() {
            let (port, ips) = socket_addrs_to_port_addrs(&v4_bound);
            mdns.republish_addrs(port, ips);
        }

        // Set relay URL TXT attribute (routed through actor, sequenced after addrs).
        mdns.set_relay_url(relay_url.map(|u| u.as_str()));

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
    ) -> Option<impl n0_future::Stream<Item = crate::network::discovery::DiscoveryEvent> + Unpin + use<>>
    {
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

#[cfg(test)]
mod tests {
    use super::*;

    /// `vault_topic` and `vault_id_from_topic` must be exact inverses so a pairing
    /// initiator can recover the mesh's VaultId from the topic it receives on the wire.
    #[test]
    fn vault_topic_round_trips_through_vault_id_from_topic() {
        // Cover edge values: small ids that zero-pad the high bytes, a full-width
        // id, and the all-ones id — the bit patterns most likely to expose a
        // byte-order or width mistake in the encode/decode pair.
        for raw in [1u64, 0xFF, 0x1234, 0xa1b2c3d4e5f67890, u64::MAX] {
            let vault_id = VaultId::from(raw);
            let topic = SyncNode::vault_topic(&vault_id);
            let recovered = SyncNode::vault_id_from_topic(topic.as_bytes());
            assert_eq!(recovered, vault_id, "round-trip failed for {raw:#x}");
        }
    }
}
