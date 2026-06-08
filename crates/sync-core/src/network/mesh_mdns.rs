//! mDNS mesh discovery actor backed by `swarm-discovery`.
//!
//! `MeshMdns` wraps a `swarm-discovery::Discoverer` configured with
//! `IpClass::V4Only`. On macOS, VPN/Tailscale tun interfaces inject multiple
//! IPv6 default routes (utun0–utun5); `IpClass::Auto` (the iroh-mdns default)
//! then tries IPv6 multicast on all of them and hits `EHOSTUNREACH` once a
//! second. Locking to V4-only eliminates that error storm.
//!
//! The actor model here mirrors `iroh-mdns-address-lookup`: a long-lived task
//! owns the peer snapshot map and fan-outs events to subscribers. The
//! `Discoverer` callback is sync, so it posts to the actor channel via a
//! one-shot async task rather than blocking.
//!
//! ## AddressLookup integration
//!
//! `MeshMdns` implements iroh's `AddressLookup` trait so that
//! `endpoint.connect(node_id, ALPN)` works without ever calling
//! `add_node_addr`. The `resolve()` method forwards to the actor which looks
//! up the peer in its snapshot map and returns an `AddressLookupItem` with the
//! peer's direct addrs and relay URL.  `publish()` extracts the relay URL and
//! IPv4 socket addresses from iroh's `EndpointData` and routes them through
//! the actor channel — this makes our relay URL visible to remote peers via mDNS
//! so they can bootstrap relay routing.  `user_data` is intentionally NOT
//! extracted in `publish()` — it is managed separately via `set_user_data` to
//! avoid the clobber path where iroh's data would overwrite our JSON.
//!
//! ## TXT ordering (Bug 2 fix)
//!
//! `set_user_data` and `set_relay_url` now send actor messages (channel sends)
//! instead of calling `guard.set_txt_attribute` directly. This guarantees that
//! swarm-discovery sees `remove_all → add → set_txt_user_data → set_txt_relay`
//! in FIFO order on a single channel, eliminating the race where `RemoveAll`
//! (triggered by `republish_addrs` via the actor) wiped the TXT attributes
//! that were already set by the sync `guard` calls.

use std::{
    collections::HashMap,
    net::{IpAddr, SocketAddr},
    sync::Arc,
};

use iroh::EndpointId;
use iroh::address_lookup::{
    AddressLookup, EndpointData as IrohEndpointData, EndpointInfo as IrohEndpointInfo,
    Error as AddressLookupError, Item as AddressLookupItem, UserData,
};
use n0_future::boxed::BoxStream;
use n0_future::task::AbortOnDropHandle;
use swarm_discovery::{Discoverer, DropGuard, IpClass, Peer};
use tokio::runtime::Handle;
use tokio::sync::mpsc::{self, error::TrySendError};
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, info, trace, warn};

use super::discovery::{DiscoveryEvent, EndpointData, EndpointInfo};

/// TXT key for the `UserData` JSON blob (matches iroh's wire format).
const USER_DATA_ATTRIBUTE: &str = "user-data";

/// TXT key for the relay URL (matches iroh's wire format).
const RELAY_URL_ATTRIBUTE: &str = "relay";

/// Provenance string reported in `AddressLookupItem`s from this service.
const MDNS_PROVENANCE: &str = "mesh-mdns";

/// Messages from the callback forwarder and `MeshMdns` handle methods to the actor.
#[derive(Debug)]
enum Message {
    /// A peer was updated or expired (empty addrs = expiry).
    Peer(String, Peer),
    /// A new subscriber wants events.
    Subscribe(mpsc::Sender<DiscoveryEvent>),
    /// Re-advertise with a new port and address list.
    RepublishAddrs(u16, Vec<IpAddr>),
    /// Set the `user-data` TXT attribute (routed through actor for ordering).
    SetUserData(String),
    /// Set the `relay` TXT attribute (routed through actor for ordering).
    SetRelayUrl(Option<String>),
    /// Resolve a peer's addresses for iroh's AddressLookup.
    Resolve(
        EndpointId,
        mpsc::Sender<Result<AddressLookupItem, AddressLookupError>>,
    ),
}

/// Manages the set of live `DiscoveryEvent` subscribers.
///
/// Dead (closed/full) subscribers are pruned lazily on each send.
struct Subscribers(Vec<mpsc::Sender<DiscoveryEvent>>);

impl Subscribers {
    fn new() -> Self {
        Self(vec![])
    }

    fn push(&mut self, sender: mpsc::Sender<DiscoveryEvent>) {
        self.0.push(sender);
    }

    fn send(&mut self, event: DiscoveryEvent) {
        let mut dead = vec![];
        for (i, s) in self.0.iter().enumerate() {
            match s.try_send(event.clone()) {
                Ok(()) => {}
                Err(TrySendError::Full(_)) => {
                    warn!(idx = i, "mDNS subscriber is full, dropping event");
                }
                Err(TrySendError::Closed(_)) => {
                    dead.push(i);
                }
            }
        }
        for i in dead.into_iter().rev() {
            self.0.swap_remove(i);
        }
    }
}

/// Handle to a live mDNS discovery actor.
///
/// Cheap to clone — all clones share the same actor channel. Drop all
/// clones (and the internal `DropGuard`) to stop discovery.
#[derive(Clone)]
pub struct MeshMdns {
    sender: mpsc::Sender<Message>,
    /// Keeps the actor alive and the `Discoverer` running.
    #[allow(dead_code)]
    handle: Arc<AbortOnDropHandle<()>>,
    /// Keeps the `Discoverer` actor running.
    #[allow(dead_code)]
    guard: Arc<DropGuard>,
}

impl std::fmt::Debug for MeshMdns {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeshMdns").finish_non_exhaustive()
    }
}

/// Implements iroh's `AddressLookup` trait so that `endpoint.connect(node_id, ALPN)`
/// resolves addresses via our mDNS peer snapshot map.
///
/// `publish()` extracts the relay URL and IP addresses from iroh's `EndpointData`
/// and routes them through the actor channel so that remote peers can discover our
/// relay URL via mDNS and use relay routing. `user_data` is intentionally NOT
/// extracted here — we manage it separately via `set_user_data`, which avoids
/// the clobber path where iroh's published data would overwrite our JSON.
impl AddressLookup for MeshMdns {
    fn publish(&self, data: &IrohEndpointData) {
        // Extract relay URL so peers can route to us via relay.
        // We pick the first relay URL if multiple are present (iroh typically uses one).
        let relay_url = data.relay_urls().next().map(|u| u.to_string());
        let _ = self.sender.try_send(Message::SetRelayUrl(relay_url));

        // Extract direct IP addresses so peers can attempt direct connections.
        // Filter to IPv4 only (matching our V4Only mDNS scope).
        let addrs: Vec<std::net::SocketAddr> = data
            .ip_addrs()
            .filter(|sa| sa.is_ipv4())
            .copied()
            .collect();
        if !addrs.is_empty() {
            let (port, ips) = socket_addrs_to_port_addrs(&addrs);
            let _ = self.sender.try_send(Message::RepublishAddrs(port, ips));
        }
    }

    fn resolve(
        &self,
        endpoint_id: EndpointId,
    ) -> Option<BoxStream<Result<AddressLookupItem, AddressLookupError>>> {
        use futures::FutureExt;

        let (send, recv) = mpsc::channel(20);
        let actor_tx = self.sender.clone();
        let stream = async move {
            actor_tx
                .send(Message::Resolve(endpoint_id, send))
                .await
                .ok();
            ReceiverStream::new(recv)
        };
        Some(Box::pin(stream.flatten_stream()))
    }
}

impl MeshMdns {
    /// Start a V4-only mDNS actor for `endpoint_id`.
    ///
    /// `service_name` scopes discovery — only peers on the same service name
    /// are visible. `port` and `addrs` are the initial advertised addresses;
    /// update them later with `republish_addrs`. `rt` is the tokio runtime
    /// handle the actor tasks are spawned on.
    ///
    /// Returns `Err` if `swarm-discovery` fails to bind (e.g., no IPv4 interface
    /// available), letting the caller fall through to `mdns: None`.
    // swarm_discovery::SpawnError is large but this is a constructor that returns
    // Ok on the hot path — boxing the rarely-taken Err would complicate the call site.
    #[allow(clippy::result_large_err)]
    pub fn new(
        endpoint_id: EndpointId,
        service_name: &str,
        port: u16,
        addrs: Vec<IpAddr>,
        rt: &Handle,
    ) -> Result<Self, swarm_discovery::SpawnError> {
        let (tx, mut rx) = mpsc::channel::<Message>(64);
        let callback_tx = tx.clone();
        let spawn_rt = rt.clone();

        let callback = move |peer_id: &str, peer: &Peer| {
            let sender = callback_tx.clone();
            let peer_id = peer_id.to_string();
            let peer = peer.clone();
            // The callback is sync; dispatch to the actor without blocking.
            spawn_rt.spawn(async move {
                sender.send(Message::Peer(peer_id, peer)).await.ok();
            });
        };

        // Encode the endpoint_id as base32 lowercase — swarm-discovery uses this
        // as the mDNS instance name, so it must be DNS-label-safe.
        let peer_id_str = data_encoding::BASE32_NOPAD
            .encode(endpoint_id.as_bytes())
            .to_ascii_lowercase();

        let guard = Discoverer::new_interactive(service_name.to_string(), peer_id_str)
            .with_callback(callback)
            .with_ip_class(IpClass::V4Only)
            .with_addrs(port, addrs.clone())
            .spawn(rt)?;

        let guard = Arc::new(guard);
        let guard_for_actor = guard.clone();
        let own_peer_id_bytes = *endpoint_id.as_bytes();

        let actor = async move {
            let mut peers: HashMap<EndpointId, Peer> = HashMap::new();
            let mut subscribers = Subscribers::new();
            // Pending resolve requests: endpoint_id → list of reply senders.
            // Senders are held open so iroh's relay path establishment can
            // satisfy the pending connect via insert_open_path, which fires
            // emit_pending_resolve_requests when any path (relay or direct) opens.
            // When we later discover the peer via mDNS, we satisfy them directly.
            let mut resolvers: HashMap<
                EndpointId,
                Vec<mpsc::Sender<Result<AddressLookupItem, AddressLookupError>>>,
            > = HashMap::new();

            loop {
                let msg = match rx.recv().await {
                    Some(m) => m,
                    None => {
                        trace!("mDNS actor channel closed");
                        return;
                    }
                };

                match msg {
                    Message::Peer(peer_id_str, peer) => {
                        // Decode base32 back to raw bytes, then reconstruct the EndpointId.
                        let raw = match data_encoding::BASE32_NOPAD
                            .decode(peer_id_str.to_ascii_uppercase().as_bytes())
                        {
                            Ok(b) if b.len() == 32 => b,
                            _ => {
                                warn!(peer_id = %peer_id_str, "mDNS: invalid base32 peer_id, skipping");
                                continue;
                            }
                        };
                        let mut bytes = [0u8; 32];
                        bytes.copy_from_slice(&raw);

                        // Skip events for our own endpoint.
                        if bytes == own_peer_id_bytes {
                            continue;
                        }

                        // Reconstruct the EndpointId from raw bytes.
                        let endpoint_id = match iroh::EndpointId::from_bytes(&bytes) {
                            Ok(id) => id,
                            Err(_) => {
                                warn!(peer_id = %peer_id_str, "mDNS: failed to parse endpoint_id bytes");
                                continue;
                            }
                        };

                        if peer.is_expiry() {
                            trace!(%endpoint_id, "mDNS: peer expired");
                            peers.remove(&endpoint_id);
                            subscribers.send(DiscoveryEvent::Expired { endpoint_id });
                            continue;
                        }

                        // Deduplicate: skip if we've already emitted this exact peer snapshot.
                        let entry = peers.entry(endpoint_id);
                        if let std::collections::hash_map::Entry::Occupied(ref occ) = entry
                            && occ.get() == &peer
                        {
                            continue;
                        }
                        entry.or_insert_with(|| peer.clone()).clone_from(&peer);

                        // Satisfy any pending resolve requests for this peer.
                        if let Some(senders) = resolvers.remove(&endpoint_id) {
                            let item = peer_to_address_lookup_item(&peer, &endpoint_id);
                            for sender in &senders {
                                sender.send(Ok(item.clone())).await.ok();
                            }
                        }

                        // Parse optional UserData from the TXT "user-data" attribute.
                        // On parse failure we log at debug and omit the field, matching
                        // iroh-mdns-address-lookup's behaviour (lines 546-558).
                        let user_data = peer
                            .txt_attribute(USER_DATA_ATTRIBUTE)
                            .and_then(|v| v)
                            .and_then(|s| match s.parse::<UserData>() {
                                Ok(ud) => Some(ud),
                                Err(e) => {
                                    debug!(%endpoint_id, "mDNS: failed to parse user-data TXT: {e}");
                                    None
                                }
                            });

                        let event = DiscoveryEvent::Discovered {
                            endpoint_info: EndpointInfo {
                                endpoint_id,
                                data: EndpointData::new(user_data),
                            },
                        };
                        subscribers.send(event);
                    }

                    Message::Subscribe(sender) => {
                        trace!("mDNS actor: new subscriber");
                        subscribers.push(sender);
                    }

                    Message::RepublishAddrs(port, addrs) => {
                        guard_for_actor.remove_all();
                        guard_for_actor.add(port, addrs);
                    }

                    Message::SetUserData(json) => {
                        if let Err(e) = guard_for_actor.set_txt_attribute(
                            USER_DATA_ATTRIBUTE.to_string(),
                            Some(json),
                        ) {
                            warn!("mDNS: failed to set user-data TXT attribute: {e}");
                        }
                    }

                    Message::SetRelayUrl(url) => {
                        if let Err(e) = guard_for_actor
                            .set_txt_attribute(RELAY_URL_ATTRIBUTE.to_string(), url)
                        {
                            warn!("mDNS: failed to set relay TXT attribute: {e}");
                        }
                    }

                    Message::Resolve(endpoint_id, sender) => {
                        // If we already have a snapshot for this peer, reply immediately.
                        if let Some(peer) = peers.get(&endpoint_id) {
                            let item = peer_to_address_lookup_item(peer, &endpoint_id);
                            sender.send(Ok(item)).await.ok();
                        } else {
                            // For unknown peers, park the sender. iroh's relay routing
                            // runs in parallel with the lookup — when the relay path
                            // becomes available, iroh satisfies pending connect futures
                            // via `insert_open_path` regardless of this stream. Holding
                            // the sender open (rather than dropping immediately) prevents
                            // iroh from treating the lookup as a hard failure (NoResults)
                            // before the relay path has had a chance to be established.
                            //
                            // Senders are dropped when the stream is dropped (connect
                            // timeout or successful connection), so they don't leak.
                            resolvers.entry(endpoint_id).or_default().push(sender);
                        }
                    }
                }
            }
        };

        let join = tokio::spawn(actor);
        let handle = Arc::new(AbortOnDropHandle::new(join));

        info!(
            service = %service_name,
            "mDNS mesh discovery started (V4Only)"
        );

        Ok(Self {
            sender: tx,
            handle,
            guard,
        })
    }

    /// Update the advertised `UserData` TXT attribute.
    ///
    /// `json` must be a valid `UserData` string (the raw JSON of `MeshMetadata`).
    /// The update is routed through the actor channel so it is sequenced after
    /// any pending `RepublishAddrs` (remove_all/add), preventing the race where
    /// `RemoveAll` wipes TXT attributes that were set synchronously before the
    /// actor processed the address update.
    pub fn set_user_data(&self, json: &str) {
        // Best-effort — ignore send errors (actor may be shutting down).
        let _ = self
            .sender
            .try_send(Message::SetUserData(json.to_string()));
    }

    /// Update or clear the advertised relay URL TXT attribute.
    ///
    /// Routed through the actor channel for the same ordering reason as `set_user_data`.
    pub fn set_relay_url(&self, url: Option<&str>) {
        let _ = self
            .sender
            .try_send(Message::SetRelayUrl(url.map(|s| s.to_string())));
    }

    /// Re-advertise with a new port and address list.
    ///
    /// Used when the endpoint rebinds to different local sockets.
    pub fn republish_addrs(&self, port: u16, addrs: Vec<IpAddr>) {
        // Best-effort — ignore send errors (actor may be shutting down).
        let _ = self.sender.try_send(Message::RepublishAddrs(port, addrs));
    }

    /// Subscribe to a stream of `DiscoveryEvent`s from this actor.
    ///
    /// Each subscriber gets an independent channel. Events for already-known peers
    /// are not replayed — only future events are delivered.
    pub async fn subscribe(&self) -> impl n0_future::Stream<Item = DiscoveryEvent> + Unpin + use<> {
        let (tx, rx) = mpsc::channel(20);
        self.sender.send(Message::Subscribe(tx)).await.ok();
        ReceiverStream::new(rx)
    }
}

/// Convert a `swarm-discovery` `Peer` snapshot into an iroh `AddressLookupItem`.
///
/// Mirrors `peer_to_discovery_item` from `iroh-mdns-address-lookup` (lines 525-568).
/// Parses the `relay` TXT attribute as a `RelayUrl` and the `user-data` TXT attribute
/// as `UserData`, silently omitting either on parse failure.
fn peer_to_address_lookup_item(peer: &Peer, endpoint_id: &EndpointId) -> AddressLookupItem {
    use iroh::{RelayUrl, TransportAddr};
    use std::collections::BTreeSet;

    let direct_addrs: BTreeSet<std::net::SocketAddr> = peer
        .addrs()
        .iter()
        .map(|(ip, port)| std::net::SocketAddr::new(*ip, *port))
        .collect();

    let relay_url: Option<RelayUrl> =
        if let Some(Some(relay_str)) = peer.txt_attribute(RELAY_URL_ATTRIBUTE) {
            match relay_str.parse() {
                Ok(url) => Some(url),
                Err(e) => {
                    debug!(%endpoint_id, "mDNS: failed to parse relay URL from TXT: {e}");
                    None
                }
            }
        } else {
            None
        };

    let user_data: Option<UserData> =
        if let Some(Some(ud_str)) = peer.txt_attribute(USER_DATA_ATTRIBUTE) {
            match ud_str.parse() {
                Ok(ud) => Some(ud),
                Err(e) => {
                    debug!(%endpoint_id, "mDNS: failed to parse user-data from TXT: {e}");
                    None
                }
            }
        } else {
            None
        };

    let addrs: Vec<TransportAddr> = direct_addrs
        .iter()
        .map(|sa| TransportAddr::Ip(*sa))
        .chain(relay_url.map(TransportAddr::Relay))
        .collect();

    let mut data = IrohEndpointData::from_iter(addrs);
    data.set_user_data(user_data);

    let endpoint_info = IrohEndpointInfo::from_parts(*endpoint_id, data);
    AddressLookupItem::new(endpoint_info, MDNS_PROVENANCE, None)
}

/// Extract the unique port→IPs mapping from a list of bound `SocketAddr`s.
///
/// `swarm-discovery` advertises per-port address lists, but iroh's
/// `bound_sockets()` returns flat `SocketAddr`s. This groups them.
pub(crate) fn socket_addrs_to_port_addrs(addrs: &[SocketAddr]) -> (u16, Vec<IpAddr>) {
    // Use the port from the first address; all sockets typically share one port.
    let port = addrs.first().map(|a| a.port()).unwrap_or(0);
    let ips: Vec<IpAddr> = addrs.iter().map(|a| a.ip()).collect();
    (port, ips)
}
