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
//! `mdns-sd` does the macOS-correct thing: wildcard bind with `SO_REUSEPORT`,
//! per-interface `IP_MULTICAST_IF` for send, per-interface `IP_ADD_MEMBERSHIP`
//! for receive, `disable_interface(IfKind::IPv6)` to kill the utun-driven v6
//! storm cleanly, and `disable_interface(IfKind::LoopbackV4)` so that
//! `enable_addr_auto` only considers real interfaces (en0/en1) and announces
//! on the LAN.
//!
//! ## Custom port (5454, not 5353)
//!
//! We bind on `MESH_MDNS_PORT` (5454) instead of the standard mDNS port (5353).
//! Even with `SO_REUSEPORT`, when macOS `mDNSResponder` is already bound to
//! `:5353`, the kernel load-balances incoming multicast across the REUSEPORT
//! group — our socket can be starved of receives while `mDNSResponder` collects
//! the wire traffic. Using 5454 makes our daemon the sole listener on its port,
//! so every announce from a peer reaches us. We never used system Bonjour
//! interop (peers talk daemon-to-daemon), so the only loss is `dns-sd` CLI
//! visibility. Both publisher and browser share one `ServiceDaemon`, so the
//! port choice applies symmetrically.
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
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
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
use tokio::sync::{mpsc::{self, error::TrySendError}, watch};
use tokio::time;
use tokio_stream::wrappers::ReceiverStream;
use tracing::{debug, info, trace, warn};

use super::discovery::{DiscoveryEvent, EndpointData, EndpointInfo};

/// TXT key for the `UserData` JSON blob (matches iroh's wire format).
const USER_DATA_ATTRIBUTE: &str = "user-data";

/// TXT key for the relay URL (matches iroh's wire format).
const RELAY_URL_ATTRIBUTE: &str = "relay";

/// Provenance string reported in `AddressLookupItem`s from this service.
const MDNS_PROVENANCE: &str = "mesh-mdns";

/// Re-query interval while at least one subscriber (the pair window) is open.
///
/// On each tick the actor calls `daemon.verify()` for all known peers (preventing
/// TTL-driven prune) and restarts the browse to elicit PTR responses from quiet
/// peers that have backed off their announcements.
const REQUERY_INTERVAL: Duration = Duration::from_millis(1500);

/// Timeout per `daemon.verify()` call. Kept well under `REQUERY_INTERVAL` so
/// that a tick that verifies several peers completes before the next tick fires.
const VERIFY_TIMEOUT: Duration = Duration::from_millis(1000);

/// UDP port for our mDNS daemon — deliberately NOT 5353.
///
/// Using a custom port makes our `ServiceDaemon` the sole listener on its
/// port, eliminating `SO_REUSEPORT` competition with macOS `mDNSResponder`.
/// We never use system Bonjour interop (peers talk daemon-to-daemon), so the
/// only loss is `dns-sd` CLI visibility. 5454 is the port `mdns-sd`'s own
/// docs recommend for this exact scenario.
const MESH_MDNS_PORT: u16 = 5454;

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
    /// Periodic re-query tick: verify known peers and restart the browse.
    ///
    /// Sent by the requery task while at least one subscriber is connected.
    Requery,
    /// Seed a peer directly into the actor's peers map.
    ///
    /// Only available in `#[cfg(test)]`. Lets tests pre-populate known peers
    /// without going through the real mDNS stack, so `Subscribe` replay
    /// behavior can be exercised in-process.
    #[cfg(test)]
    SeedPeerForTest(EndpointId, PeerSnapshot),
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

    /// Prune closed (dropped) subscriber channels without sending an event.
    ///
    /// Used to detect subscriber count changes eagerly — without waiting for the
    /// next broadcast — so the re-query lifecycle can respond immediately when
    /// the last subscriber drops.
    fn prune(&mut self) {
        self.0.retain(|s| !s.is_closed());
    }

    fn is_empty(&self) -> bool {
        self.0.is_empty()
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
    /// Signals the bridge task to swap to a new flume receiver after a browse restart.
    #[allow(dead_code)]
    browse_recv_tx: Arc<watch::Sender<mdns_sd::Receiver<ServiceEvent>>>,
    /// Shared flag for the requery lifecycle. True while at least one subscriber
    /// is connected and the re-query task is running. Exposed for testing only.
    #[cfg(test)]
    requery_active: Arc<AtomicBool>,
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
        let daemon = ServiceDaemon::new_with_port(MESH_MDNS_PORT)?;

        // Disable IPv6 to prevent EHOSTUNREACH storms from utun-injected
        // IPv6 default routes (Tailscale / VPN interfaces on macOS).
        if let Err(e) = daemon.disable_interface(IfKind::IPv6) {
            warn!("mDNS: failed to disable IPv6 interfaces: {e}");
        }
        // Disable the loopback IPv4 interface (127.0.0.1). mdns-sd enables it by
        // default, so enable_addr_auto() picks up 127.0.0.1 and the daemon
        // announces on loopback only — peers on other machines never see the
        // service. Same-machine plugin ↔ daemon pairing is file-based (.sync/),
        // not mDNS, so we never need loopback discovery.
        if let Err(e) = daemon.disable_interface(IfKind::LoopbackV4) {
            warn!("mDNS: failed to disable LoopbackV4 interface: {e}");
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
        //
        // The watch channel lets the requery path (handled by the actor on
        // Message::Requery) swap the bridge to a fresh flume receiver after
        // calling stop_browse + browse. The bridge's tokio::select! drains the
        // current receiver and switches when a new one arrives via watch.changed().
        // flume's recv_async() is cancel-safe, so no events are lost mid-drain.
        let browse_recv = daemon.browse(&service_type)?;
        let (browse_recv_tx, browse_recv_rx) = watch::channel(browse_recv);
        let browse_recv_tx = Arc::new(browse_recv_tx);
        let bridge_tx = tx.clone();
        let bridge_join = rt.spawn(async move {
            let mut recv_watch = browse_recv_rx;
            let mut current_recv = recv_watch.borrow_and_update().clone();
            loop {
                tokio::select! {
                    // Drain the current flume receiver.
                    result = current_recv.recv_async() => {
                        match result {
                            Ok(event) => {
                                if bridge_tx.send(Message::Peer(event)).await.is_err() {
                                    return;
                                }
                            }
                            // Flume channel closed — actor is shutting down.
                            Err(_) => return,
                        }
                    }
                    // Actor sent a replacement receiver after a browse restart.
                    Ok(()) = recv_watch.changed() => {
                        current_recv = recv_watch.borrow_and_update().clone();
                    }
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

        // Clone the browse_recv_tx so the actor can hand new receivers to the bridge.
        let actor_browse_recv_tx = Arc::clone(&browse_recv_tx);
        // Clone tx so the actor can spawn child tasks with a sender clone while
        // the original tx is returned in the MeshMdns handle.
        let actor_tx_clone = tx.clone();

        // Controls the active re-query task. The Arc is shared between the actor
        // and (in test builds) the MeshMdns handle for observability.
        // Set to `true` when the first subscriber connects; `false` when the last
        // drops. The requery task polls this flag on each tick and exits when false.
        let requery_active_for_actor = Arc::new(AtomicBool::new(false));
        #[cfg(test)]
        let requery_active_shared = Arc::clone(&requery_active_for_actor);

        let actor = async move {
            let tx = actor_tx_clone;
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

            let requery_active = requery_active_for_actor;

            // Handle to the requery task — kept here so it's aborted if the
            // actor exits before the last subscriber drops.
            let mut _requery_handle: Option<AbortOnDropHandle<()>> = None;

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
                        trace!(known_peers = peers.len(), "mDNS actor: new subscriber");
                        // Replay every cached peer to the new subscriber before adding it
                        // to the broadcast set. A replayed peer may be momentarily stale
                        // (its TTL could expire between this push and the next re-query
                        // tick); that's acceptable — the re-query loop (Fix C) reconciles
                        // within ~1.5s via a `ServiceRemoved` → `Expired` event, and the
                        // pair UI tolerates an `Expired` following a `Discovered`.
                        // Replay-then-push ordering ensures the subscriber sees every
                        // known peer at least once and does not miss concurrent live events.
                        for (endpoint_id, snapshot) in &peers {
                            let event = discovered_event_from_snapshot(*endpoint_id, snapshot);
                            if sender.try_send(event).is_err() {
                                // Subscriber dropped before replay finished; abandon push.
                                break;
                            }
                        }
                        subscribers.push(sender);

                        // Start the re-query task on the first subscriber.
                        if !requery_active.load(Ordering::Relaxed) {
                            requery_active.store(true, Ordering::Relaxed);
                            let flag = Arc::clone(&requery_active);
                            let actor_tx = tx.clone();
                            let handle = AbortOnDropHandle::new(tokio::spawn(async move {
                                let mut interval =
                                    time::interval(REQUERY_INTERVAL);
                                // First tick fires immediately; skip it — we just
                                // replayed the cache so a re-query this instant adds
                                // no value and causes an unnecessary browse restart.
                                interval.tick().await;
                                loop {
                                    interval.tick().await;
                                    if !flag.load(Ordering::Relaxed) {
                                        break;
                                    }
                                    if actor_tx.send(Message::Requery).await.is_err() {
                                        break;
                                    }
                                }
                            }));
                            _requery_handle = Some(handle);
                        }
                    }

                    Message::Requery => {
                        // Verify all known peers so mdns-sd doesn't prune them on TTL expiry.
                        for endpoint_id in peers.keys() {
                            let instance = data_encoding::BASE32_NOPAD
                                .encode(endpoint_id.as_bytes())
                                .to_ascii_lowercase();
                            let fullname = format!("{instance}.{service_type}");
                            if let Err(e) = daemon.verify(fullname, VERIFY_TIMEOUT) {
                                trace!(%endpoint_id, "mDNS: verify failed: {e}");
                            }
                        }
                        // Restart the browse to reset PTR backoff and elicit fresh
                        // responses from peers that have gone quiet.
                        let _ = daemon.stop_browse(&service_type);
                        match daemon.browse(&service_type) {
                            Ok(new_recv) => {
                                // Hand the new receiver to the bridge task.
                                let _ = actor_browse_recv_tx.send(new_recv);
                            }
                            Err(e) => {
                                warn!("mDNS: re-browse failed: {e}");
                            }
                        }
                        trace!("mDNS: re-query tick complete");
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

                    #[cfg(test)]
                    Message::SeedPeerForTest(endpoint_id, snapshot) => {
                        peers.insert(endpoint_id, snapshot.clone());
                        // Emit a Discovered event to any current subscribers, mimicking
                        // what a live ServiceResolved event would do. When used for
                        // pre-seeding (before subscribe), no subscribers exist yet so
                        // this is a no-op.
                        let event = discovered_event_from_snapshot(endpoint_id, &snapshot);
                        subscribers.send(event);
                    }
                }

                // After processing each message, eagerly prune closed subscriber
                // channels and stop the re-query task when the last one drops.
                // `prune()` catches drops that didn't go through `send()`.
                if requery_active.load(Ordering::Relaxed) {
                    subscribers.prune();
                    if subscribers.is_empty() {
                        requery_active.store(false, Ordering::Relaxed);
                        _requery_handle = None;
                        trace!("mDNS: last subscriber dropped, re-query stopped");
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
            browse_recv_tx,
            #[cfg(test)]
            requery_active: requery_active_shared,
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
    /// Each subscriber gets an independent channel. Already-known peers are
    /// replayed immediately to the new subscriber before any future events are
    /// delivered, so callers always see the full current peer set. While at least
    /// one subscriber is connected the actor also runs an active re-query loop
    /// (1.5s ticks) that re-announces quiet peers and restarts the PTR browse,
    /// ensuring peers that have backed off their announcements are re-discovered
    /// quickly. The loop stops when the last subscriber drops.
    pub async fn subscribe(&self) -> impl n0_future::Stream<Item = DiscoveryEvent> + Unpin + use<> {
        let (tx, rx) = mpsc::channel(20);
        self.sender.send(Message::Subscribe(tx)).await.ok();
        ReceiverStream::new(rx)
    }

    /// Create a `MeshMdns` backed by a real daemon for in-process testing.
    ///
    /// Only available in `#[cfg(test)]`. Tests that only exercise Subscribe replay
    /// never trigger `Requery`, so the daemon exists but stays idle during those
    /// tests. Use `seed_peer_for_test()` and `requery_is_active()` to drive tests
    /// without touching the real mDNS stack.
    #[cfg(test)]
    pub(crate) fn new_for_test(
        endpoint_id: EndpointId,
        rt: &Handle,
    ) -> Result<Self, mdns_sd::Error> {
        Self::new(endpoint_id, "obsidian-sync-test", 0, vec![], rt)
    }

    /// Seed a peer directly into the actor's peer map, bypassing mDNS.
    ///
    /// Only available in `#[cfg(test)]`. If any subscribers are active when
    /// this is called, they will receive a `Discovered` event for the peer.
    #[cfg(test)]
    async fn seed_peer_for_test(&self, endpoint_id: EndpointId, snapshot: PeerSnapshot) {
        self.sender
            .send(Message::SeedPeerForTest(endpoint_id, snapshot))
            .await
            .ok();
    }

    /// Returns true if the re-query task is currently running.
    ///
    /// Only available in `#[cfg(test)]`.
    #[cfg(test)]
    fn requery_is_active(&self) -> bool {
        self.requery_active.load(Ordering::Relaxed)
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

    // Only include real IPv4 addresses — filter out 0.0.0.0 (unspecified).
    // endpoint.bound_sockets() on a wildcard-bound iroh endpoint returns
    // 0.0.0.0:port; mdns-sd (correctly) refuses to announce 0.0.0.0 as
    // on-subnet for any interface, so passing it through yields an empty
    // address set and the service is invisible to mDNSResponder.
    // Pass as &[IpAddr] — AsIpAddrs is implemented for &[I] where I: AsIpAddrs,
    // and IpAddr implements AsIpAddrs; Ipv4Addr alone does not.
    let v4_addrs: Vec<IpAddr> = addrs
        .iter()
        .filter(|ip| ip.is_ipv4() && !ip.is_unspecified())
        .copied()
        .collect();

    // hostname is "<instance_name>.local." — a valid DNS label derived from the
    // base32-encoded endpoint ID, which is always ASCII alphanumeric + lowercase.
    let host_name = format!("{instance_name}.local.");

    // enable_addr_auto() instructs mdns-sd to auto-populate the host's real
    // per-interface IPs (e.g. en1 → 192.168.68.59) and auto-update on IP change.
    // Combined with the unspecified-filter above, this ensures we always advertise
    // a reachable address even when the caller provides 0.0.0.0.
    ServiceInfo::new(
        service_type,
        instance_name,
        &host_name,
        v4_addrs.as_slice(),
        port,
        Some(properties),
    )
    .map(|info| info.enable_addr_auto())
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

    // Log parse failures for observability before building the event via the
    // shared helper. The helper silently swallows failures on the replay path
    // (the daemon already logged on first sight), but we still want this signal
    // on the live resolve path.
    if let Some(s) = user_data_str.as_deref()
        && s.parse::<UserData>().is_err()
    {
        debug!(%endpoint_id, "mDNS: failed to parse user-data TXT");
    }

    let event = discovered_event_from_snapshot(endpoint_id, &snapshot);
    subscribers.send(event);

    trace!(%endpoint_id, "mDNS: peer resolved");
}

// ---------------------------------------------------------------------------
// Helper: build a DiscoveryEvent::Discovered from a cached snapshot
// ---------------------------------------------------------------------------

/// Build a `DiscoveryEvent::Discovered` from a cached `PeerSnapshot`.
///
/// Shared by the live resolve path (`handle_service_resolved`) and the
/// `Subscribe` replay path so the two cannot drift on `user_data` parsing
/// semantics. Parse errors are silently swallowed here — callers that want
/// the error logged (the live path) do so before calling this helper.
fn discovered_event_from_snapshot(endpoint_id: EndpointId, snapshot: &PeerSnapshot) -> DiscoveryEvent {
    let user_data = snapshot.user_data.as_deref().and_then(|s| s.parse::<UserData>().ok());
    DiscoveryEvent::Discovered {
        endpoint_info: EndpointInfo {
            endpoint_id,
            data: EndpointData::new(user_data),
        },
    }
}

// ---------------------------------------------------------------------------
// Helper: decode instance name
// ---------------------------------------------------------------------------

/// Strip the macOS mDNS conflict-resolution suffix ` (N)` from an instance name.
///
/// When a daemon restarts and its chosen name is already in use, mDNSResponder
/// appends a suffix like ` (2)` — e.g. `qnca5x…sq (2)`. Without stripping it,
/// base32 decoding fails and the peer is silently dropped. The stripping is
/// intentionally conservative: the suffix must be at the very end, the parens
/// must contain only ASCII digits, and there must be at least one digit.
fn strip_conflict_suffix(name: &str) -> &str {
    let Some(idx) = name.rfind(" (") else {
        return name;
    };
    let inner = &name[idx + 2..];
    let Some(close) = inner.find(')') else {
        return name;
    };
    // Suffix must end exactly at the close paren.
    if close + 1 != inner.len() {
        return name;
    }
    // The digits between the parens must be non-empty and all ASCII decimal.
    let digits = &inner[..close];
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return name;
    }
    &name[..idx]
}

/// Decode a base32-lowercase instance name back to an `EndpointId`.
///
/// Strips any trailing macOS mDNS conflict suffix (` (N)`) before decoding
/// so that peers that got conflict-renamed after a daemon restart are not
/// silently dropped.
fn decode_instance_name(instance_name: &str) -> Result<EndpointId, ()> {
    let cleaned = strip_conflict_suffix(instance_name);
    let raw = data_encoding::BASE32_NOPAD
        .decode(cleaned.to_ascii_uppercase().as_bytes())
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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use std::net::{IpAddr, Ipv4Addr, SocketAddr};
    use std::time::Duration;
    use n0_future::StreamExt;
    use tokio::runtime::Handle;

    use iroh::{EndpointId, SecretKey};

    use super::{
        strip_conflict_suffix, DiscoveryEvent, MeshMdns, PeerSnapshot,
    };

    // Helper: generate a deterministic test EndpointId from a seed byte.
    // SecretKey::from_bytes accepts any 32 bytes; the resulting public key is
    // the EndpointId.
    fn test_endpoint_id(seed: u8) -> EndpointId {
        let bytes = [seed; 32];
        SecretKey::from_bytes(&bytes).public()
    }

    // Helper: a snapshot with user_data set.
    fn snapshot_with_data(user_data_str: &str) -> PeerSnapshot {
        PeerSnapshot {
            addrs: vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::new(192, 168, 1, 1)), 9000)],
            relay_url: None,
            user_data: Some(user_data_str.to_string()),
        }
    }

    // Helper: a snapshot without user_data.
    fn snapshot_without_data() -> PeerSnapshot {
        PeerSnapshot {
            addrs: vec![SocketAddr::new(IpAddr::V4(Ipv4Addr::new(10, 0, 0, 1)), 9001)],
            relay_url: Some("https://relay.example.com".to_string()),
            user_data: None,
        }
    }

    #[test]
    fn test_strip_conflict_suffix() {
        // Normal conflict suffix is stripped.
        assert_eq!(strip_conflict_suffix("qnca5xsq (2)"), "qnca5xsq");

        // No suffix — returned unchanged.
        assert_eq!(strip_conflict_suffix("qnca5xsq"), "qnca5xsq");

        // Non-digit content inside parens — not a conflict suffix.
        assert_eq!(strip_conflict_suffix("qnca5xsq (abc)"), "qnca5xsq (abc)");

        // Empty parens — not a conflict suffix.
        assert_eq!(strip_conflict_suffix("qnca5xsq ()"), "qnca5xsq ()");

        // Suffix is not at the end — not stripped.
        assert_eq!(
            strip_conflict_suffix("qnca5xsq (2) extra"),
            "qnca5xsq (2) extra"
        );

        // Larger conflict number works too.
        assert_eq!(strip_conflict_suffix("qnca5xsq (123)"), "qnca5xsq");
    }

    /// Subscribe replays every peer cached before the subscribe call. Newly
    /// arriving peers (via a live ServiceResolved) also flow through after replay.
    #[tokio::test]
    async fn subscribe_replays_known_peers() {
        let rt = Handle::current();
        let own_id = test_endpoint_id(0xAA);
        let mdns = MeshMdns::new_for_test(own_id, &rt).expect("daemon started");

        let peer_with_data = test_endpoint_id(0x01);
        let peer_without_data = test_endpoint_id(0x02);
        let user_data_str = r#"{"mesh":"Test Vault","vid":"0000000000000000","ver":1}"#;

        // Seed two peers into the actor before subscribing.
        mdns.seed_peer_for_test(peer_with_data, snapshot_with_data(user_data_str)).await;
        mdns.seed_peer_for_test(peer_without_data, snapshot_without_data()).await;

        // Subscribe after the seeds arrive; the actor should replay both.
        let mut stream = mdns.subscribe().await;

        let mut seen_ids = std::collections::HashSet::new();
        let mut user_data_round_tripped = false;

        // Collect exactly 2 events (the 2 replayed peers).
        for _ in 0..2 {
            let event = tokio::time::timeout(Duration::from_secs(2), stream.next())
                .await
                .expect("timed out waiting for replay event")
                .expect("stream ended early");

            match event {
                DiscoveryEvent::Discovered { endpoint_info } => {
                    let id = endpoint_info.endpoint_id;
                    seen_ids.insert(id);
                    if id == peer_with_data {
                        // user_data must round-trip through the snapshot parse.
                        let ud = endpoint_info.data.user_data().expect("user_data present");
                        assert_eq!(ud.as_ref(), user_data_str);
                        user_data_round_tripped = true;
                    }
                }
                DiscoveryEvent::Expired { .. } => panic!("unexpected Expired in replay"),
            }
        }

        assert!(seen_ids.contains(&peer_with_data), "peer_with_data not replayed");
        assert!(seen_ids.contains(&peer_without_data), "peer_without_data not replayed");
        assert!(user_data_round_tripped, "user_data did not round-trip");
    }

    /// Replay events arrive before any concurrent live events, preserving the
    /// "replay-then-push" ordering invariant documented in the Subscribe handler.
    #[tokio::test]
    async fn subscribe_replay_then_live_event_ordering() {
        let rt = Handle::current();
        let own_id = test_endpoint_id(0xBB);
        let mdns = MeshMdns::new_for_test(own_id, &rt).expect("daemon started");

        let seeded_peer = test_endpoint_id(0x10);
        let live_peer = test_endpoint_id(0x11);
        let user_data_str = r#"{"mesh":"Order Test","vid":"1111111111111111","ver":1}"#;

        // Seed one peer before subscribing.
        mdns.seed_peer_for_test(seeded_peer, snapshot_with_data(user_data_str)).await;

        // Subscribe — the seeded peer should be replayed first.
        let mut stream = mdns.subscribe().await;

        // Immediately after subscribe, inject a live peer via SeedPeerForTest
        // (standing in for a live ServiceResolved event).
        mdns.seed_peer_for_test(live_peer, snapshot_without_data()).await;

        // First event must be the replayed seeded_peer, not the live_peer.
        let first_event = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("timed out waiting for first event")
            .expect("stream ended early");

        match first_event {
            DiscoveryEvent::Discovered { endpoint_info } => {
                assert_eq!(
                    endpoint_info.endpoint_id, seeded_peer,
                    "first event must be the replayed seeded peer"
                );
            }
            DiscoveryEvent::Expired { .. } => panic!("unexpected Expired event first"),
        }

        // Second event must be the live_peer (seeded after subscribe).
        let second_event = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await
            .expect("timed out waiting for second event")
            .expect("stream ended early");

        match second_event {
            DiscoveryEvent::Discovered { endpoint_info } => {
                assert_eq!(
                    endpoint_info.endpoint_id, live_peer,
                    "second event must be the live peer"
                );
            }
            DiscoveryEvent::Expired { .. } => panic!("unexpected Expired event second"),
        }
    }

    /// The re-query task starts when the first subscriber connects and stops
    /// when the last subscriber drops. Subscribing again after all subscribers
    /// have dropped restarts the task.
    #[tokio::test]
    async fn requery_starts_on_first_subscribe_and_stops_on_last_unsubscribe() {
        let rt = Handle::current();
        let own_id = test_endpoint_id(0xCC);
        let mdns = MeshMdns::new_for_test(own_id, &rt).expect("daemon started");

        // Initially inactive — no subscribers yet.
        assert!(!mdns.requery_is_active(), "requery should be inactive before any subscriber");

        // Subscribe — task should start.
        let stream1 = mdns.subscribe().await;
        // The channel send completes once the value lands in the buffer, but the
        // actor processes it asynchronously. Poll the flag with a short timeout.
        wait_for(&mdns, true, "requery should be active after first subscriber").await;

        // Drop the only subscriber. The actor's end-of-loop prune() call detects
        // the closed channel on the next message it processes.
        drop(stream1);

        // Trigger the actor to process a message so it runs the prune check.
        let trigger_peer = test_endpoint_id(0x20);
        mdns.seed_peer_for_test(trigger_peer, snapshot_without_data()).await;
        wait_for(&mdns, false, "requery should be inactive after last subscriber dropped").await;

        // Subscribing again restarts the task.
        let _stream2 = mdns.subscribe().await;
        wait_for(&mdns, true, "requery should restart on new subscriber").await;
    }

    /// Poll `mdns.requery_is_active()` until it equals `expected`, up to 100ms.
    async fn wait_for(mdns: &MeshMdns, expected: bool, msg: &str) {
        for _ in 0..20 {
            if mdns.requery_is_active() == expected {
                return;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
        assert_eq!(mdns.requery_is_active(), expected, "{msg}");
    }
}
