//! mDNS mesh discovery actor backed by `mdns-sd`.
//!
//! `MeshMdns` wraps a `mdns_sd::ServiceDaemon` configured for IPv4-only operation
//! on macOS-hostile networks. The previous `swarm-discovery` backend had three
//! independent macOS problems:
//!
//! - Per-interface specific-IP binds collide with mDNSResponder on `:5353` (EADDRINUSE).
//! - `INADDR_ANY` join roulettes between IPv4 interfaces on multi-interface hosts.
//! - IPv6 join storms `EHOSTUNREACH` on hosts with utun-injected IPv6 default routes.
//!
//! `mdns-sd` does the macOS-correct thing: wildcard `0.0.0.0:5353` with
//! `SO_REUSEPORT`, per-interface `IP_MULTICAST_IF` for send, per-interface
//! `IP_ADD_MEMBERSHIP` for receive, and `disable_interface(IfKind::IPv6)` to
//! kill the utun-driven v6 storm cleanly.
//!
//! ## Architecture
//!
//! A long-lived tokio task (the "actor") owns all mutable state. All public
//! methods send messages to it via an mpsc channel. The actor:
//!
//! - Maintains canonical state: `last_user_data`, `last_relay_url`, `last_port`,
//!   `last_addrs`.
//! - On every config change (any of the four fields), rebuilds a **complete**
//!   `ServiceInfo` from all current state and calls `daemon.register()`. Per the
//!   mdns-sd docs, calling `register()` again re-announces with updated info;
//!   no `unregister` is needed. This makes the user-data clobber bug structurally
//!   impossible: every register carries every field.
//! - Bridges `mdns_sd::ServiceDaemon::browse()` — which returns a flume `Receiver`
//!   driven by the daemon's internal OS thread — into the tokio actor via a spawned
//!   bridge task using `recv_async().await`.
//!
//! ## AddressLookup integration
//!
//! `MeshMdns` implements iroh's `AddressLookup` trait so that
//! `endpoint.connect(node_id, ALPN)` works without ever calling `add_node_addr`.
//! `publish()` extracts the relay URL and IPv4 socket addresses from iroh's
//! `EndpointData` and routes them through the actor channel. `user_data` is
//! intentionally NOT extracted in `publish()` — it is managed separately via
//! `set_user_data` to prevent iroh's auto-publish from clobbering our JSON.

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
use mdns_sd::{IfKind, ServiceDaemon, ServiceEvent, ServiceInfo};
use n0_future::boxed::BoxStream;
use n0_future::task::AbortOnDropHandle;
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

/// Messages sent to the actor from the public API and the browse bridge.
#[derive(Debug)]
enum Message {
    /// A browse event from the `ServiceDaemon` (forwarded by the bridge task).
    Peer(ServiceEvent),
    /// A new subscriber wants future `DiscoveryEvent`s.
    Subscribe(mpsc::Sender<DiscoveryEvent>),
    /// Re-advertise with a new port and address list.
    RepublishAddrs(u16, Vec<IpAddr>),
    /// Update the `user-data` TXT attribute.
    SetUserData(String),
    /// Update or clear the `relay` TXT attribute.
    SetRelayUrl(Option<String>),
    /// Resolve a peer's addresses for iroh's `AddressLookup`.
    Resolve(
        EndpointId,
        mpsc::Sender<Result<AddressLookupItem, AddressLookupError>>,
    ),
}

/// Snapshot of a resolved peer, stored in the actor's peer map.
#[derive(Debug, Clone)]
struct PeerSnapshot {
    /// IPv4 socket addresses the peer is listening on.
    addrs: Vec<SocketAddr>,
    /// Optional relay URL for relay-routed connections.
    relay_url: Option<String>,
    /// Raw user-data TXT string (unparsed; callers parse on demand).
    user_data: Option<String>,
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
/// clones to stop discovery (the actor task will exit when the channel closes).
#[derive(Clone)]
pub struct MeshMdns {
    sender: mpsc::Sender<Message>,
    /// Keeps the actor task alive.
    #[allow(dead_code)]
    handle: Arc<AbortOnDropHandle<()>>,
    /// Keeps the browse bridge task alive.
    #[allow(dead_code)]
    bridge_handle: Arc<AbortOnDropHandle<()>>,
}

impl std::fmt::Debug for MeshMdns {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("MeshMdns").finish_non_exhaustive()
    }
}

/// Implements iroh's `AddressLookup` trait so that `endpoint.connect(node_id, ALPN)`
/// resolves addresses via our mDNS peer snapshot map.
///
/// `publish()` extracts the relay URL and IPv4 socket addresses from iroh's
/// `EndpointData` and routes them through the actor channel so that remote peers
/// can discover our relay URL via mDNS and use relay routing. `user_data` is
/// intentionally NOT extracted here — we manage it separately via `set_user_data`,
/// which avoids the clobber path where iroh's published data would overwrite our JSON.
impl AddressLookup for MeshMdns {
    fn publish(&self, data: &IrohEndpointData) {
        // Extract relay URL so peers can route to us via relay.
        // We pick the first relay URL if multiple are present (iroh typically uses one).
        let relay_url = data.relay_urls().next().map(|u| u.to_string());
        let _ = self.sender.try_send(Message::SetRelayUrl(relay_url));

        // Extract direct IPv4 addresses so peers can attempt direct connections.
        let addrs: Vec<SocketAddr> = data
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
    /// Returns `Err` if `mdns-sd` fails to start (e.g., no IPv4 interface
    /// available), letting the caller fall through to `mdns: None`.
    pub fn new(
        endpoint_id: EndpointId,
        service_name: &str,
        port: u16,
        addrs: Vec<IpAddr>,
        rt: &Handle,
    ) -> Result<Self, mdns_sd::Error> {
        let daemon = ServiceDaemon::new()?;

        // Disable IPv6 to prevent EHOSTUNREACH storms from utun-injected
        // IPv6 default routes (Tailscale / VPN interfaces on macOS).
        if let Err(e) = daemon.disable_interface(IfKind::IPv6) {
            warn!("mDNS: failed to disable IPv6 interfaces: {e}");
        }

        let service_type = format!("_{service_name}._udp.local.");

        // Encode the endpoint_id as base32 lowercase — this becomes the mDNS
        // instance name, so it must be DNS-label-safe and decodable on browse.
        let instance_name = data_encoding::BASE32_NOPAD
            .encode(endpoint_id.as_bytes())
            .to_ascii_lowercase();

        let (tx, mut rx) = mpsc::channel::<Message>(64);

        // Spawn the bridge task: pumps flume Receiver<ServiceEvent> (driven by
        // the daemon's internal OS thread) into our tokio actor mpsc channel.
        // recv_async() is flume's async adapter — it does NOT block the runtime.
        let browse_recv = daemon.browse(&service_type)?;
        let bridge_tx = tx.clone();
        let bridge_join = rt.spawn(async move {
            while let Ok(event) = browse_recv.recv_async().await {
                if bridge_tx.send(Message::Peer(event)).await.is_err() {
                    break;
                }
            }
        });
        let bridge_handle = Arc::new(AbortOnDropHandle::new(bridge_join));

        let own_peer_id_bytes = *endpoint_id.as_bytes();

        // Build and register the initial ServiceInfo.
        let initial_service_info = build_service_info(
            &service_type,
            &instance_name,
            port,
            &addrs,
            None,
            None,
        );
        match initial_service_info {
            Ok(info) => {
                if let Err(e) = daemon.register(info) {
                    warn!("mDNS: initial register failed: {e}");
                }
            }
            Err(e) => {
                warn!("mDNS: failed to build initial ServiceInfo: {e}");
            }
        }

        let actor = async move {
            let mut peers: HashMap<EndpointId, PeerSnapshot> = HashMap::new();
            let mut subscribers = Subscribers::new();
            // Pending resolve requests: endpoint_id → list of reply senders.
            // Holding senders open (rather than replying with NoResults) gives
            // iroh's relay path a chance to open — when the relay path becomes
            // available, iroh satisfies connect futures via insert_open_path
            // regardless of this stream. Senders are dropped when the stream is
            // dropped (connect timeout or successful connection), so they don't leak.
            let mut resolvers: HashMap<
                EndpointId,
                Vec<mpsc::Sender<Result<AddressLookupItem, AddressLookupError>>>,
            > = HashMap::new();

            // Canonical state: the actor rebuilds a complete ServiceInfo from
            // all four fields on every config change.
            let mut last_user_data: Option<String> = None;
            let mut last_relay_url: Option<String> = None;
            let mut last_port: u16 = port;
            let mut last_addrs: Vec<IpAddr> = addrs;

            loop {
                let msg = match rx.recv().await {
                    Some(m) => m,
                    None => {
                        trace!("mDNS actor channel closed");
                        return;
                    }
                };

                match msg {
                    Message::Peer(event) => {
                        match event {
                            ServiceEvent::ServiceResolved(info) => {
                                handle_service_resolved(
                                    &info,
                                    own_peer_id_bytes,
                                    &mut peers,
                                    &mut resolvers,
                                    &mut subscribers,
                                )
                                .await;
                            }

                            ServiceEvent::ServiceRemoved(_ty, fullname) => {
                                // Extract the instance name from the fullname
                                // (everything before the first dot).
                                let instance_part = fullname.split('.').next().unwrap_or("");
                                match decode_instance_name(instance_part) {
                                    Ok(endpoint_id) => {
                                        if *endpoint_id.as_bytes() == own_peer_id_bytes {
                                            continue;
                                        }
                                        trace!(%endpoint_id, "mDNS: service removed");
                                        peers.remove(&endpoint_id);
                                        subscribers
                                            .send(DiscoveryEvent::Expired { endpoint_id });
                                    }
                                    Err(_) => {
                                        debug!(
                                            fullname = %fullname,
                                            "mDNS: ServiceRemoved with unrecognised instance name, ignoring"
                                        );
                                    }
                                }
                            }

                            ServiceEvent::SearchStarted(ty) => {
                                debug!("mDNS: search started for {ty}");
                            }

                            _ => {
                                // ServiceFound, SearchStopped — informational only.
                            }
                        }
                    }

                    Message::Subscribe(sender) => {
                        trace!("mDNS actor: new subscriber");
                        subscribers.push(sender);
                    }

                    Message::RepublishAddrs(p, a) => {
                        last_port = p;
                        last_addrs = a;
                        reregister(
                            &daemon,
                            &service_type,
                            &instance_name,
                            last_port,
                            &last_addrs,
                            last_user_data.as_deref(),
                            last_relay_url.as_deref(),
                        );
                    }

                    Message::SetUserData(json) => {
                        last_user_data = Some(json);
                        reregister(
                            &daemon,
                            &service_type,
                            &instance_name,
                            last_port,
                            &last_addrs,
                            last_user_data.as_deref(),
                            last_relay_url.as_deref(),
                        );
                    }

                    Message::SetRelayUrl(url) => {
                        last_relay_url = url;
                        reregister(
                            &daemon,
                            &service_type,
                            &instance_name,
                            last_port,
                            &last_addrs,
                            last_user_data.as_deref(),
                            last_relay_url.as_deref(),
                        );
                    }

                    Message::Resolve(endpoint_id, sender) => {
                        // If we already have a snapshot for this peer, reply immediately.
                        if let Some(snapshot) = peers.get(&endpoint_id) {
                            let item = snapshot_to_address_lookup_item(snapshot, &endpoint_id);
                            sender.send(Ok(item)).await.ok();
                        } else {
                            // Park the sender until the peer is discovered via mDNS.
                            resolvers.entry(endpoint_id).or_default().push(sender);
                        }
                    }
                }
            }
        };

        let join = rt.spawn(actor);
        let handle = Arc::new(AbortOnDropHandle::new(join));

        info!(
            service = %service_name,
            "mDNS mesh discovery started (IPv4-only, mdns-sd)"
        );

        Ok(Self {
            sender: tx,
            handle,
            bridge_handle,
        })
    }

    /// Update the advertised `user-data` TXT attribute.
    ///
    /// `json` must be a valid `UserData` string (the raw JSON of `MeshMetadata`).
    /// Routed through the actor channel so it is sequenced with any concurrent
    /// `RepublishAddrs` calls — prevents TXT state from diverging due to ordering.
    pub fn set_user_data(&self, json: &str) {
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

// ---------------------------------------------------------------------------
// Helper: re-register with the canonical state
// ---------------------------------------------------------------------------

/// Build a fresh `ServiceInfo` from canonical state and call `daemon.register()`.
///
/// Per the mdns-sd docs: calling `register()` again re-announces with the updated
/// `ServiceInfo`; no `unregister` is needed. Every field is always included, so
/// there is no remove-then-set window where attributes can be lost.
fn reregister(
    daemon: &ServiceDaemon,
    service_type: &str,
    instance_name: &str,
    port: u16,
    addrs: &[IpAddr],
    user_data: Option<&str>,
    relay_url: Option<&str>,
) {
    match build_service_info(service_type, instance_name, port, addrs, user_data, relay_url) {
        Ok(info) => {
            if let Err(e) = daemon.register(info) {
                warn!("mDNS: re-register failed: {e}");
            }
        }
        Err(e) => {
            warn!("mDNS: failed to build ServiceInfo for re-register: {e}");
        }
    }
}

/// Build a `ServiceInfo` from canonical state.
///
/// Properties always contain both `user-data` and `relay` keys. When a value
/// is `None`, the key is omitted (an empty property map entry would be
/// confusing to readers).
fn build_service_info(
    service_type: &str,
    instance_name: &str,
    port: u16,
    addrs: &[IpAddr],
    user_data: Option<&str>,
    relay_url: Option<&str>,
) -> Result<ServiceInfo, mdns_sd::Error> {
    let mut properties: HashMap<String, String> = HashMap::new();
    if let Some(ud) = user_data {
        properties.insert(USER_DATA_ATTRIBUTE.to_string(), ud.to_string());
    }
    if let Some(url) = relay_url {
        properties.insert(RELAY_URL_ATTRIBUTE.to_string(), url.to_string());
    }

    // Only include IPv4 addresses in the advertisement (matching our IPv4-only scope).
    // Pass as &[IpAddr] — AsIpAddrs is implemented for &[I] where I: AsIpAddrs,
    // and IpAddr implements AsIpAddrs; Ipv4Addr alone does not.
    let v4_addrs: Vec<IpAddr> = addrs
        .iter()
        .filter(|ip| ip.is_ipv4())
        .copied()
        .collect();

    // hostname is "<instance_name>.local." — a valid DNS label derived from the
    // base32-encoded endpoint ID, which is always ASCII alphanumeric + lowercase.
    let host_name = format!("{instance_name}.local.");

    ServiceInfo::new(
        service_type,
        instance_name,
        &host_name,
        v4_addrs.as_slice(),
        port,
        Some(properties),
    )
}

// ---------------------------------------------------------------------------
// Helper: handle a resolved service event
// ---------------------------------------------------------------------------

async fn handle_service_resolved(
    info: &mdns_sd::ResolvedService,
    own_peer_id_bytes: [u8; 32],
    peers: &mut HashMap<EndpointId, PeerSnapshot>,
    resolvers: &mut HashMap<
        EndpointId,
        Vec<mpsc::Sender<Result<AddressLookupItem, AddressLookupError>>>,
    >,
    subscribers: &mut Subscribers,
) {
    // The instance name is everything in the fullname before the first dot.
    let fullname = info.get_fullname();
    let instance_part = fullname.split('.').next().unwrap_or("");

    let endpoint_id = match decode_instance_name(instance_part) {
        Ok(id) => id,
        Err(_) => {
            warn!(
                fullname = %fullname,
                "mDNS: ServiceResolved with unrecognised instance name, ignoring"
            );
            return;
        }
    };

    // Skip events for our own endpoint.
    if *endpoint_id.as_bytes() == own_peer_id_bytes {
        return;
    }

    let user_data_str = info
        .get_property_val_str(USER_DATA_ATTRIBUTE)
        .map(|s| s.to_string());

    let relay_url_str = info
        .get_property_val_str(RELAY_URL_ATTRIBUTE)
        .map(|s| s.to_string());

    // Collect IPv4 addresses from the resolved service.
    let port = info.get_port();
    let v4_addrs: Vec<SocketAddr> = info
        .get_addresses_v4()
        .iter()
        .map(|ip| SocketAddr::new(IpAddr::V4(*ip), port))
        .collect();

    let snapshot = PeerSnapshot {
        addrs: v4_addrs,
        relay_url: relay_url_str,
        user_data: user_data_str.clone(),
    };

    // Deduplicate: skip if the snapshot is identical to what we already have.
    if peers.get(&endpoint_id).is_some_and(|existing| {
        existing.addrs == snapshot.addrs
            && existing.relay_url == snapshot.relay_url
            && existing.user_data == snapshot.user_data
    }) {
        return;
    }

    peers.insert(endpoint_id, snapshot.clone());

    // Satisfy any pending resolve requests for this peer.
    if let Some(senders) = resolvers.remove(&endpoint_id) {
        let item = snapshot_to_address_lookup_item(&snapshot, &endpoint_id);
        for sender in &senders {
            sender.send(Ok(item.clone())).await.ok();
        }
    }

    // Parse the user-data TXT string into iroh's UserData type.
    // On parse failure we log at debug and omit the field, matching
    // iroh-mdns-address-lookup's behaviour.
    let user_data: Option<UserData> = user_data_str.as_deref().and_then(|s| {
        match s.parse::<UserData>() {
            Ok(ud) => Some(ud),
            Err(e) => {
                debug!(%endpoint_id, "mDNS: failed to parse user-data TXT: {e}");
                None
            }
        }
    });

    let event = DiscoveryEvent::Discovered {
        endpoint_info: EndpointInfo {
            endpoint_id,
            data: EndpointData::new(user_data),
        },
    };
    subscribers.send(event);

    trace!(%endpoint_id, "mDNS: peer resolved");
}

// ---------------------------------------------------------------------------
// Helper: decode instance name
// ---------------------------------------------------------------------------

/// Decode a base32-lowercase instance name back to an `EndpointId`.
fn decode_instance_name(instance_name: &str) -> Result<EndpointId, ()> {
    let raw = data_encoding::BASE32_NOPAD
        .decode(instance_name.to_ascii_uppercase().as_bytes())
        .map_err(|_| ())?;
    if raw.len() != 32 {
        return Err(());
    }
    let mut bytes = [0u8; 32];
    bytes.copy_from_slice(&raw);
    iroh::EndpointId::from_bytes(&bytes).map_err(|_| ())
}

// ---------------------------------------------------------------------------
// Helper: convert snapshot to AddressLookupItem
// ---------------------------------------------------------------------------

/// Convert a `PeerSnapshot` into an iroh `AddressLookupItem`.
fn snapshot_to_address_lookup_item(
    snapshot: &PeerSnapshot,
    endpoint_id: &EndpointId,
) -> AddressLookupItem {
    use iroh::{RelayUrl, TransportAddr};

    let relay_url: Option<RelayUrl> = snapshot.relay_url.as_deref().and_then(|s| {
        match s.parse() {
            Ok(url) => Some(url),
            Err(e) => {
                debug!(%endpoint_id, "mDNS: failed to parse relay URL from snapshot: {e}");
                None
            }
        }
    });

    let user_data: Option<UserData> = snapshot.user_data.as_deref().and_then(|s| {
        match s.parse() {
            Ok(ud) => Some(ud),
            Err(e) => {
                debug!(%endpoint_id, "mDNS: failed to parse user-data from snapshot: {e}");
                None
            }
        }
    });

    let addrs: Vec<TransportAddr> = snapshot
        .addrs
        .iter()
        .map(|sa| TransportAddr::Ip(*sa))
        .chain(relay_url.map(TransportAddr::Relay))
        .collect();

    let mut data = IrohEndpointData::from_iter(addrs);
    data.set_user_data(user_data);

    let endpoint_info = IrohEndpointInfo::from_parts(*endpoint_id, data);
    AddressLookupItem::new(endpoint_info, MDNS_PROVENANCE, None)
}

// ---------------------------------------------------------------------------
// Utility (public for node.rs)
// ---------------------------------------------------------------------------

/// Extract the unique port→IPs mapping from a list of bound `SocketAddr`s.
///
/// iroh's `bound_sockets()` returns flat `SocketAddr`s. This groups them
/// by picking the port from the first address (all sockets typically share
/// one port) and collecting all IPs.
pub(crate) fn socket_addrs_to_port_addrs(addrs: &[SocketAddr]) -> (u16, Vec<IpAddr>) {
    let port = addrs.first().map(|a| a.port()).unwrap_or(0);
    let ips: Vec<IpAddr> = addrs.iter().map(|a| a.ip()).collect();
    (port, ips)
}
