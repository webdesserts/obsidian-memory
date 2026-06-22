//! Daemon event loop: the `Daemon` struct, its fields, and all event handlers.
//!
//! Submodule layout:
//! - `startup` — startup seam (`StartupBundle`, `startup_inner`) and public entry points
//!   (`run_with_shutdown_controlled`, `run_with_shutdown`, `run`).
//! - `initiator` — two-step GUI pairing state machine (`InitiatorSession`,
//!   `InitiatorPairOutcome`, `run_initiator_pairing_parked`, and the five
//!   `impl Daemon` initiator methods).

mod initiator;
mod startup;
mod sync_exchange;
mod sync_stream;

// Re-export public names so `sync_daemon::daemon::X` paths stay byte-identical.
use initiator::{InitiatorPairOutcome, InitiatorSession};
pub use startup::{run, run_with_shutdown, run_with_shutdown_controlled};
use sync_exchange::{SyncExchangeKind, spawn_sync_exchange};
// Re-exported for the integration-test harness, which builds nodes with the same
// pumped inbound handler production uses (see `sync_stream`).
pub use sync_stream::PumpedSyncHandler;

use crate::move_coalescer::{Expired, MoveCoalescer, MoveDecision};
use crate::pair_api::{ConnectionState, DaemonCommand, DaemonStatus, PairingUiEvent, PeerSummary};
use crate::relay_class::relay_is_offlan_reachable;
use crate::watcher::{FileEvent, FileEventKind};
use iroh::{EndpointAddr, EndpointId};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::{broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use sync_core::allowlist::{AllowedPeer, AllowlistStorage};
use sync_core::network::{
    GOSSIP_ALPN, SyncNode,
    gossip::{GossipEvent, GossipMessage, MAX_GOSSIP_MESSAGE_SIZE, VaultGossip},
    pairing::{InboundPairingExchange, PairingApproval, PairingEvent},
};
use sync_core::pairing::{PairingChallenge, PairingSession};
use sync_core::time_scale::{scaled, scaled_ms};
use sync_core::{PeerId, PeerRegistry};
// The vault/sync-engine layer is vault-sync; the iroh half stays on sync-core.
// `Daemon<FS>` and `Vault<FS>` are both generic over vault-sync's `FileSystem`.
use vault_sync::Vault;
use vault_sync::fs::FileSystem;

pub(crate) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// First reconnect attempt fires ~5s after a daemon drops to zero neighbors —
/// fast enough that a brief flap heals quickly, slow enough to let a transient
/// blip self-resolve before the supervisor acts.
const RECONNECT_BASE_MS: u64 = 5_000;

/// Base interval for a fresh/briefly-unreachable hint's reconnect attempts.
///
/// `60s` matches the old global reconnect ceiling, so a healthy hint retries on
/// the same cadence the supervisor used before per-hint backoff existed — no
/// behavior change for the common transient-drop case.
const HINT_BACKOFF_BASE_MS: u64 = 60_000;

/// Shift ceiling for the per-hint backoff. `60s << 5 ≈ 32 min`, which bounds
/// growth and keeps the shift well clear of overflow.
const HINT_BACKOFF_CEIL: u32 = 5;

/// Maximum per-hint backoff. A dead hint is re-added + dialed roughly twice an
/// hour — frequent enough to catch a returning peer, rare enough that the
/// failed-dial burst is negligible noise (vs the continuous loop we're fixing).
const MAX_HINT_BACKOFF_MS: u64 = 1_800_000;

/// Consecutive failed attempts after which a hint is considered "stale": it
/// gets the max-backoff cadence and the one-shot eviction log. Low enough to
/// quiet noise within ~an hour of a peer going dark, high enough not to throttle
/// a few-minute flaky outage.
const STALE_FAILURE_THRESHOLD: u32 = 5;

/// How often the connected-path allowlist reconcile re-pushes the membership
/// roster to live neighbors. The reconnect supervisor wakes every
/// [`RECONNECT_BASE_MS`] (5s), but the roster push is throttled to this slower
/// cadence so a healthy mesh re-converges any drift (a missed `NeighborUp`, a
/// restart) without a per-tick gossip broadcast. ~60s is responsive at the
/// stated 3-machine scale and negligible fan-out.
const ROSTER_RECONCILE_MS: u64 = 60_000;

/// Per-hint exponential backoff window from a hint's consecutive failure count.
///
/// `HINT_BACKOFF_BASE_MS << min(failure_count, HINT_BACKOFF_CEIL)`, saturating
/// at [`MAX_HINT_BACKOFF_MS`]. Throttles a dead hint's attempt cadence WITHOUT
/// abandoning it — even the stalest hint is re-dialed once per returned window.
fn per_hint_backoff(failure_count: u32) -> u64 {
    let shift = failure_count.min(HINT_BACKOFF_CEIL);
    // Scale both the per-failure window and the ceiling so the test-time-scale
    // shrinks the backoff cadence consistently (D4/D5). At scale 1.0 (prod) both
    // calls return their input unchanged.
    scaled_ms(HINT_BACKOFF_BASE_MS << shift).min(scaled_ms(MAX_HINT_BACKOFF_MS))
}

/// Whether a hint is due for another reconnect attempt at `now_ms`.
///
/// A never-attempted hint (`last_attempt_ms == None`) is due immediately — we
/// have never tried it, so there is nothing to back off from. For an
/// already-attempted hint the elapsed math uses `saturating_sub` so a backward
/// clock jump (a `last_attempt_ms` in the future) yields `0` elapsed — the hint
/// reads not-yet-due for one window, then becomes due again, never permanently
/// wedged.
fn hint_attempt_due(hint: &crate::persistence::PeerRelay, now_ms: u64) -> bool {
    match hint.last_attempt_ms {
        None => true,
        Some(last_attempt) => {
            now_ms.saturating_sub(last_attempt) >= per_hint_backoff(hint.failure_count)
        }
    }
}

/// Decide whether a relay learned from a peer should be adopted into this node's
/// public-relay set (C6 gossip expansion).
///
/// Once connected, gossip carries each peer's home relay; a server's home relay
/// IS its public relay, so a laptop can discover a second server's relay it was
/// never paired with and add it for failover redundancy. We adopt only when the
/// learned relay is:
/// - present (a LAN-direct exchange yields `None` — nothing to learn), AND
/// - off-LAN-reachable (a private LAN-IP relay is useless once we leave that LAN,
///   so it must never become a home candidate — mirrors the `add_known_public_relay`
///   classifier guard), AND
/// - not already in `known_public_relays` (the idempotency guard: iroh exposes no
///   public `Endpoint::contains_relay()`, so the persisted set stands in for
///   RelayMap membership — every relay we insert we also persist and vice-versa —
///   preventing per-exchange RelayMap churn and duplicate cross-product entries).
fn learned_public_relay_to_adopt(
    learned: Option<&iroh::RelayUrl>,
    known_public_relays: &[String],
) -> Option<iroh::RelayUrl> {
    let url = learned?;
    let url_str = url.to_string();
    if !relay_is_offlan_reachable(&url_str) {
        return None;
    }
    if known_public_relays.iter().any(|u| u == &url_str) {
        return None;
    }
    Some(url.clone())
}

/// Configuration for running the sync daemon.
pub struct DaemonRunConfig {
    pub vault: PathBuf,
    /// Optional path to an alternate identity key file (default: `.sync/daemon.key`).
    pub identity_key: Option<PathBuf>,
    /// If set, serve a `/health` endpoint on this address (e.g. `"127.0.0.1:8081"`).
    pub health_listen: Option<String>,
    /// If set, start an embedded iroh relay server on this address (e.g. `"0.0.0.0:3340"`).
    ///
    /// Relay startup failure is non-fatal — the daemon will log a warning and continue
    /// without relay support rather than refusing to start.
    pub relay_listen: Option<String>,
    /// The URL advertised to peers for the embedded relay.
    ///
    /// When `relay_listen` binds to `0.0.0.0`, the bound address is not dialable
    /// by peers. Set this to the machine's LAN IP so peers can reach the relay.
    /// Defaults to the bound address when `None`.
    pub advertised_relay_url: Option<String>,
}

/// A peer's freshness, observed from a successful background sync exchange.
///
/// `spawn_sync_exchange` runs in a detached task with no `&mut self`, so it
/// can't refresh the supervisor snapshot directly. It sends this back to the
/// run-loop (mirroring the initiator-outcome channel) where `on_exchange_learned`
/// applies it — resetting the hint's throttle the instant a peer is reachable,
/// which is what makes stale-hint eviction safe.
struct ExchangeLearned {
    /// The peer we just synced with.
    endpoint_id: EndpointId,
    /// The active relay URL iroh reported for the peer, if it connected through
    /// a relay. `None` for a LAN-direct connection (no relay path) — we still
    /// stamp success, we just don't overwrite the stored URL with nothing.
    relay_url: Option<iroh::RelayUrl>,
}

/// Daemon state holding all components.
///
/// Generic over the filesystem (`FS`) and allowlist storage (`AL`) so that
/// integration tests can inject in-memory implementations without touching the
/// real filesystem or spawning a file watcher.
pub struct Daemon<FS: FileSystem, AL> {
    vault: Arc<Mutex<Vault<FS>>>,
    /// Exposed `pub(crate)` so `startup.rs` can call `sync_node.shutdown()`
    /// (which consumes `self`) during graceful teardown without requiring a
    /// separate accessor method.
    pub(crate) sync_node: SyncNode,
    vault_gossip: VaultGossip,
    /// Injected file event channel — replaces the `FileWatcher` field so tests can
    /// push synthetic events without a real OS filesystem watcher.
    file_event_rx: mpsc::UnboundedReceiver<FileEvent>,
    /// Optional mDNS discovery channel. `None` in tests (and on WASM builds).
    discovery_rx: Option<mpsc::Receiver<sync_core::network::discovery::DiscoveredMesh>>,
    /// Network-change signal. Each `()` means iroh observed an `EndpointAddr`
    /// change (relay/direct-address delta ⇒ network conditions changed). Drained
    /// in the run_loop select; the handler resets every hint's backoff so the
    /// next supervisor tick re-dials on the new network — the fix for "every net
    /// change needs a restart." `None` in tests that don't exercise reconnect,
    /// and on any build where `watch_addr` isn't wired.
    net_change_rx: Option<mpsc::Receiver<()>>,
    /// Inbound-sync freshness signal. The pumped inbound handler
    /// (`sync_stream::PumpedSyncHandler`) processes a peer's sync inline and
    /// fires that peer's id here on completion; the run-loop drains it and stamps
    /// `peer_registry.update_last_seen`. The handler can't touch `peer_registry`
    /// directly — it's built before `Daemon` exists — so this fire-and-forget
    /// channel carries the inbound liveness stamp instead (S2). It is the
    /// inbound-only-peer liveness path that the `alive_count` broadcast gate
    /// reads. `None` (inert) until wired post-construction by `startup.rs` or a
    /// test; an undrained channel is harmless (the handler's `send` fails quietly).
    inbound_seen_rx: Option<mpsc::UnboundedReceiver<PeerId>>,
    /// Tracks liveness state of all peers observed in the gossip swarm.
    ///
    /// Wrapped in `Arc<Mutex<...>>` so spawned tasks (QUIC sync exchanges) can
    /// call `update_last_seen` after a successful sync without borrowing `&mut self`.
    peer_registry: Arc<Mutex<PeerRegistry>>,
    /// Controls which peers are allowed to sync. Empty = deny all (pair first).
    ///
    /// Wrapped in Arc so it can be shared with the gossip connection handler,
    /// which runs in separate tasks and must check the allowlist on each inbound
    /// gossip connection before the gossip protocol runs.
    allowlist: Arc<AL>,
    /// Active pairing session, if any. Only one pairing session at a time.
    active_pairing: Option<PairingSession>,
    /// Human-readable name for this device, advertised during pairing.
    device_name: String,
    /// Relay URL if the embedded relay is running, for distribution to new peers.
    relay_url: Option<String>,
    /// The vault's filesystem path. Used by the post-pair onboarding to persist
    /// the adopted relay URL to `daemon.toml`.
    vault_path: PathBuf,
    /// Cancellation token wired to the process shutdown signal. Calling
    /// `shutdown.cancel()` causes `run_loop` to exit cleanly.
    shutdown: CancellationToken,
    /// Broadcasts live daemon status to the desktop tray. `None` when running
    /// from the CLI (no tray to update).
    status_tx: Option<watch::Sender<DaemonStatus>>,
    /// Broadcasts pairing UI events to the desktop tray. `None` in CLI mode.
    pairing_tx: Option<broadcast::Sender<PairingUiEvent>>,
    /// Receives commands from the desktop tray. `None` in CLI mode.
    command_rx: Option<mpsc::UnboundedReceiver<DaemonCommand>>,
    /// Name of the vault mesh, carried through for status broadcasts.
    mesh_name: Option<String>,
    /// In-flight initiator pairing session. Holds the discovered meshes map so
    /// `SubmitCode` can resolve `vault_id` → `EndpointId` without re-scanning,
    /// and a cancellation token so `CancelInitiate` aborts both the discovery
    /// task and any pairing attempt in progress. `None` between sessions.
    active_initiator: Option<InitiatorSession>,
    /// Sender half of the internal initiator-outcome channel. Cloned into the
    /// spawned pairing task so a completed `PairingResult` can be routed back to
    /// the event loop for `&mut self` onboarding work (adopt + re-join).
    initiator_outcome_tx: mpsc::UnboundedSender<InitiatorPairOutcome>,
    /// Receiver half, drained in a `run_loop` `select!` arm.
    initiator_outcome_rx: mpsc::UnboundedReceiver<InitiatorPairOutcome>,
    /// Sender half of the learn-on-exchange channel. Cloned into each spawned
    /// sync-exchange task so a successful sync routes the peer's observed relay
    /// back to the event loop for `&mut self` snapshot/persist refresh.
    exchange_learn_tx: mpsc::UnboundedSender<ExchangeLearned>,
    /// Receiver half, drained in a `run_loop` `select!` arm.
    exchange_learn_rx: mpsc::UnboundedReceiver<ExchangeLearned>,

    // ── reconnect supervisor ────────────────────────────────────────────────
    /// Steady wake cadence for the reconnect supervisor. Per-hint backoff
    /// (tracked on each `PeerRelay`'s freshness fields) decides whether a given
    /// hint actually attempts a re-dial on a wake — there is no longer a single
    /// global backoff gate.
    reconnect_tick: tokio::time::Interval,
    /// In-memory snapshot of persisted peer relay hints, the supervisor's source
    /// of truth for who to re-dial. Populated at startup from `DaemonConfig` and
    /// updated when pairing learns a peer's relay. Held in memory (not reloaded
    /// from disk per tick) so the test harness — which uses a non-existent vault
    /// path — can seed it directly without writing `daemon.toml`.
    peer_relays: Vec<crate::persistence::PeerRelay>,
    /// EndpointIds with a supervisor-issued bootstrap connect currently in flight.
    ///
    /// When a peer's first gossip bootstrap dial parks (relay-only, off-LAN), the
    /// supervisor establishes the connection itself and hands it to gossip (see
    /// `on_reconnect_tick`). Those connects are spawned tasks that can take up to
    /// the QUIC handshake timeout to fail, while the supervisor keeps ticking
    /// every ~200ms after a net change — so without a guard the same peer would
    /// accumulate dozens of concurrent connects. An id is inserted before its
    /// task spawns and removed when the task finishes; a tick skips any id already
    /// present. Shared `Arc<tokio::sync::Mutex<…>>` because the removal happens
    /// inside the spawned task, which can't borrow `&mut self`. The async mutex
    /// (not `std`) is required: nothing holds the guard across an `.await` today,
    /// but it lives behind the same shared-handle pattern as `peer_registry`.
    bootstrap_connect_inflight: Arc<Mutex<HashSet<EndpointId>>>,

    // ── allowlist convergence ───────────────────────────────────────────────
    /// Wall-clock (`now_ms()`) of the last connected-path roster reconcile, or
    /// `None` if one hasn't fired yet. Throttles the roster push inside
    /// `on_reconnect_tick` to [`ROSTER_RECONCILE_MS`] so it doesn't broadcast on
    /// every 5s supervisor wake.
    last_roster_reconcile_ms: Option<u64>,
    /// Reconcile throttle window (defaults to [`ROSTER_RECONCILE_MS`]). A field
    /// rather than a const only so integration tests can shrink it to observe a
    /// reconcile within `wait_until`'s budget — same test-seam rationale as
    /// [`Daemon::set_reconnect_interval`].
    roster_reconcile_interval_ms: u64,

    // ── native-move coalescing (P4f-1) ──────────────────────────────────────
    /// Buffers not-yet-tracked create/delete events for a short window so a native
    /// rename (which the watcher delivers as an unlinked delete+create — see
    /// `move_coalescer`) collapses into one same-UUID move instead of a tombstone
    /// plus a fresh-UUID create. Pure (no vault handle); `on_file_changed` computes
    /// the content hashes and executes the decisions it returns.
    move_coalescer: MoveCoalescer,
    /// Drives the coalescer's window-expiry sweep on a steady cadence — a buffered
    /// event whose partner never arrived commits to its standalone meaning (a real
    /// tombstone or a real new doc) once its window passes.
    move_sweep_tick: tokio::time::Interval,
}

impl<FS: FileSystem + 'static, AL: AllowlistStorage + 'static> Daemon<FS, AL> {
    /// Construct a daemon from pre-built components.
    ///
    /// The `run()` function creates these from real filesystem paths; tests
    /// inject in-memory equivalents directly.
    pub fn new(
        vault: Arc<Mutex<Vault<FS>>>,
        sync_node: SyncNode,
        vault_gossip: VaultGossip,
        file_event_rx: mpsc::UnboundedReceiver<FileEvent>,
        discovery_rx: Option<mpsc::Receiver<sync_core::network::discovery::DiscoveredMesh>>,
        allowlist: Arc<AL>,
        device_name: String,
        relay_url: Option<String>,
        vault_path: PathBuf,
        shutdown: CancellationToken,
    ) -> Self {
        // The initiator-outcome and learn-on-exchange channels are internal —
        // created here rather than passed in so the public `Daemon::new`
        // signature (used directly by the test harness) stays unchanged.
        let (initiator_outcome_tx, initiator_outcome_rx) = mpsc::unbounded_channel();
        let (exchange_learn_tx, exchange_learn_rx) = mpsc::unbounded_channel();

        // The reconnect supervisor wakes on a steady cadence; backoff (tracked in
        // the fields below) gates whether a wake actually re-dials. `Delay` skips
        // missed ticks rather than bursting to catch up after a slow turn.
        let mut reconnect_tick =
            tokio::time::interval(scaled(std::time::Duration::from_millis(RECONNECT_BASE_MS)));
        reconnect_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        // The move-coalescer sweep wakes on a fraction of the pairing window so an
        // unpaired event's standalone commit is caught promptly. `Delay` skips
        // missed ticks rather than bursting after a slow turn.
        let mut move_sweep_tick = tokio::time::interval(MoveCoalescer::sweep_interval());
        move_sweep_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        Self {
            vault,
            sync_node,
            vault_gossip,
            file_event_rx,
            discovery_rx,
            // Wired post-construction by the startup `watch_addr` task (or by
            // tests via `set_net_change_rx`); `None` means net-change reconnect
            // is inert, exactly like an unwired `discovery_rx`.
            net_change_rx: None,
            // Wired post-construction (startup.rs / tests via `set_inbound_seen_rx`);
            // `None` means inbound freshness stamping is inert, like an unwired
            // `net_change_rx`.
            inbound_seen_rx: None,
            peer_registry: Arc::new(Mutex::new(PeerRegistry::new())),
            allowlist,
            active_pairing: None,
            device_name,
            relay_url,
            vault_path,
            shutdown,
            status_tx: None,
            pairing_tx: None,
            command_rx: None,
            mesh_name: None,
            active_initiator: None,
            initiator_outcome_tx,
            initiator_outcome_rx,
            exchange_learn_tx,
            exchange_learn_rx,
            reconnect_tick,
            peer_relays: Vec::new(),
            bootstrap_connect_inflight: Arc::new(Mutex::new(HashSet::new())),
            last_roster_reconcile_ms: None,
            roster_reconcile_interval_ms: scaled_ms(ROSTER_RECONCILE_MS),
            move_coalescer: MoveCoalescer::new(),
            move_sweep_tick,
        }
    }

    /// Wire control channels into this daemon, enabling tray integration.
    ///
    /// Called by `run_with_shutdown_controlled` after `Daemon::new()` succeeds
    /// but before `run_loop` starts. Not called by the CLI entry points.
    pub fn wire_control(
        &mut self,
        status_tx: watch::Sender<DaemonStatus>,
        pairing_tx: broadcast::Sender<PairingUiEvent>,
        command_rx: mpsc::UnboundedReceiver<DaemonCommand>,
        mesh_name: String,
    ) {
        self.status_tx = Some(status_tx);
        self.pairing_tx = Some(pairing_tx);
        self.command_rx = Some(command_rx);
        self.mesh_name = Some(mesh_name);
    }

    /// Compute and broadcast the current daemon status on the watch channel.
    ///
    /// No-op when running in CLI mode (no `status_tx`).
    pub async fn emit_status(&self) {
        let Some(ref status_tx) = self.status_tx else {
            return;
        };

        let registry = self.peer_registry.lock().await;
        let alive_peers = registry.get_alive_peers();
        let peer_count = alive_peers.len();
        let state = if peer_count > 0 {
            ConnectionState::Connected
        } else {
            ConnectionState::Idle
        };

        let peers = alive_peers
            .iter()
            .map(|e| PeerSummary {
                device_name: e.device_name.clone(),
                last_seen: e.last_seen,
            })
            .collect();

        let status = DaemonStatus {
            state,
            peer_count,
            peers,
            relay_url: self.relay_url.clone(),
            mesh_name: self.mesh_name.clone(),
            device_name: Some(self.device_name.clone()),
        };

        // send() only fails if all receivers are dropped — not an error condition.
        let _ = status_tx.send(status);
    }

    /// Emit a pairing UI event to the desktop tray.
    ///
    /// No-op when running in CLI mode (no `pairing_tx`).
    fn emit_pairing_event(&self, event: PairingUiEvent) {
        let Some(ref pairing_tx) = self.pairing_tx else {
            return;
        };
        // send() only fails when there are no receivers — not an error condition.
        let _ = pairing_tx.send(event);
    }

    /// Run the daemon event loop until the shutdown token is cancelled.
    ///
    /// This is the `loop { tokio::select! { ... } }` body, extracted so tests can
    /// drive it in a background task and inject synthetic events via channels.
    pub async fn run_loop(&mut self) {
        loop {
            tokio::select! {
                Some(event) = self.file_event_rx.recv() => {
                    self.on_file_changed(event).await;
                }

                Some(gossip_event) = self.vault_gossip.event_rx.recv() => {
                    match gossip_event {
                        GossipEvent::NeighborUp(node_id) => {
                            self.on_neighbor_up(node_id).await;
                        }
                        GossipEvent::NeighborDown(node_id) => {
                            self.on_neighbor_down(node_id).await;
                        }
                        GossipEvent::ChangeReceived { from, notification } => {
                            self.on_change_received(from, notification.path).await;
                        }
                        GossipEvent::AllowlistUpdate { from, peer } => {
                            self.on_allowlist_update_received(from, peer).await;
                        }
                        GossipEvent::AllowlistRoster { from, peers } => {
                            self.on_allowlist_roster_received(from, peers).await;
                        }
                    }
                }

                Some(remote_id) = async {
                    match self.inbound_seen_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => None,
                    }
                } => {
                    // A peer completed an inbound sync (handled inline by the pumped
                    // handler). Stamp its liveness so the broadcast `alive_count`
                    // gate counts an inbound-only peer (S2) — the inbound handler
                    // can't reach `peer_registry`, so it routes the stamp here.
                    self.peer_registry.lock().await.update_last_seen(&remote_id);
                }

                Some(pairing_event) = self.sync_node.inbound_pairing_rx.recv() => {
                    match pairing_event {
                        PairingEvent::InboundRequest(exchange) => {
                            self.on_inbound_pairing_request(exchange).await;
                        }
                        PairingEvent::PairingCompleted { peer_id, device_name } => {
                            self.on_pairing_completed(peer_id, device_name).await;
                        }
                        PairingEvent::PairingFailed { peer_id, reason } => {
                            self.on_pairing_failed(peer_id, reason);
                        }
                    }
                }

                Some(mesh) = async {
                    match self.discovery_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => None,
                    }
                } => {
                    info!(
                        mesh = %mesh.mesh_name,
                        vid = %mesh.vault_id,
                        peers = mesh.online_count,
                        "mDNS: discovered mesh on LAN"
                    );
                }

                Some(()) = async {
                    match self.net_change_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => None,
                    }
                } => {
                    self.on_network_change().await;
                }

                Some(cmd) = async {
                    match self.command_rx.as_mut() {
                        Some(rx) => rx.recv().await,
                        None => None,
                    }
                } => {
                    self.on_daemon_command(cmd).await;
                }

                Some(outcome) = self.initiator_outcome_rx.recv() => {
                    self.on_initiator_pair_outcome(outcome).await;
                }

                Some(learned) = self.exchange_learn_rx.recv() => {
                    self.on_exchange_learned(learned).await;
                }

                _ = self.reconnect_tick.tick() => {
                    self.on_reconnect_tick().await;
                }

                _ = self.move_sweep_tick.tick() => {
                    self.sweep_pending_moves().await;
                }

                _ = self.shutdown.cancelled() => {
                    info!("Shutting down");
                    break;
                }
            }
        }
    }

    /// Reconnect supervisor: heal a partition by re-dialing peer relay hints,
    /// without a process restart — now with per-hint backoff and eviction.
    ///
    /// Why this exists (it is NOT reinventing iroh): iroh-gossip does not re-dial
    /// bootstrap peers once a node's active AND passive views both empty — a total
    /// partition leaves no internal mechanism that dials anything; only an
    /// app-issued `Join` re-injects bootstrap peers. iroh also keepalives live
    /// connections but never re-dials a DROPPED peer, so re-dial cadence is entirely
    /// app policy. This supervisor is that policy. See [[iroh vs Hand-Rolled
    /// Connectivity]] (KEEP verdict) before assuming it's redundant.
    ///
    /// Runs on a steady tick. There is no single global backoff gate: each hint
    /// carries its own freshness (`failure_count` / `last_attempt_ms`) and is
    /// throttled independently. The state machine:
    ///
    /// 1. A live neighbor → connected. Return (no log spam, no churn).
    /// 2. No known peer relays → nothing to chase (unpaired or hint-less). Return.
    /// 3. For each hint, `hint_attempt_due` decides:
    ///    - **Due:** re-seed its relay (`set_peer_relay`) and re-bootstrap gossip
    ///      toward its EndpointId. An unparseable URL skips the seed but STILL
    ///      bootstraps by EndpointId — mDNS or a prior direct address may reach
    ///      it on-LAN. `last_attempt_ms` is stamped now.
    ///    - **Not due (throttled):** EVICT the hint from the address-lookup
    ///      (`remove_peer_relay`) so iroh-gossip's HyParView stops re-resolving
    ///      and re-feeding the dead relay to iroh's relay actor — UNLESS it is
    ///      the only hint we have OR the only off-LAN-reachable hint, in which
    ///      case it is RETAINED (see the sole-hint / off-LAN-lifeline rule below).
    /// 4. `rejoin_peers` with just the due hints. A successful re-dial surfaces
    ///    as a normal `NeighborUp` → `on_neighbor_up` → full sync.
    /// 5. Since step 1 already returned when connected, reaching here means zero
    ///    neighbors — so every hint dialed this tick failed. Bump `failure_count`
    ///    (in memory + persisted, so the throttle survives a restart) and emit
    ///    the one-shot eviction log when a hint first crosses the stale threshold.
    ///
    /// **Last-hint guarantee:** a due hint is ALWAYS re-added regardless of how
    /// high its `failure_count` is (the per-hint backoff caps, so even the
    /// stalest hint comes due within [`MAX_HINT_BACKOFF_MS`]). Eviction only ever
    /// removes the in-memory lookup entry, never the durable `PeerRelay` row — a
    /// genuinely off-LAN peer that returns is still reached on the slow cadence,
    /// and learn-on-exchange resets it the instant it reconnects.
    ///
    /// **Sole-hint / off-LAN-lifeline rule (relay-reap fix):** while partitioned,
    /// a throttled hint is never evicted if it is the ONLY hint we have OR the
    /// ONLY off-LAN-reachable one (a public/domain relay, classified by
    /// [`relay_is_offlan_reachable`]). Throttling caps how often we DIAL it, but
    /// its address must stay resident so a reaped non-home relay actor can be
    /// respawned and the partition can heal without a process restart. Evicting
    /// the only address is what caused the 12+ minute non-recovery this rule
    /// exists to prevent; the off-LAN generalization additionally keeps a laptop's
    /// public-relay lifeline resident when its other hints are dead LAN-IP relays
    /// (the n0-DNS-removal regression). A throttled hint is still evicted when a
    /// real alternative remains — another hint when LAN-only protection applies,
    /// or another off-LAN route.
    ///
    /// **Net-change interaction:** [`on_network_change`] un-throttles every hint
    /// (flipping it back to *due*) so the next tick re-seeds rather than evicts.
    /// Together with this off-LAN-lifeline retention, the public-relay lifeline
    /// survives a network switch from both sides: it is never evicted while
    /// throttled, and a net change re-dials it immediately on the new network.
    async fn on_reconnect_tick(&mut self) {
        // Step 1: connected — stay quiet and don't churn the swarm.
        //
        // The reconnect/dial logic below is the shipped, validated supervisor and
        // is untouched. The ONLY addition on the connected path is a throttled
        // allowlist-roster reconcile: re-converge any membership drift (a missed
        // NeighborUp, a restart) on a slow cadence. It is gated behind its own
        // timer so it fires ~once/ROSTER_RECONCILE_MS, not on every 5s wake, and
        // it does not touch the dial/hint machinery — when connected we still
        // return before reaching it.
        if self.peer_registry.lock().await.alive_count() > 0 {
            self.maybe_reconcile_allowlist_roster().await;
            return;
        }

        // Step 2: no hints to chase.
        if self.peer_relays.is_empty() {
            return;
        }

        let now = now_ms();
        let mut bootstrap_ids: Vec<EndpointId> = Vec::new();
        // EndpointIds dialed this tick, so we can attribute the (coarse) failure
        // after the attempt if no neighbor materializes.
        let mut due_ids: Vec<EndpointId> = Vec::new();
        // Due hints whose relay URL parses, paired with that URL. Drives the
        // supervisor-issued relay-carrying connect below — the recovery for peers
        // whose bare-id gossip dial parked and can never be re-dialed by gossip.
        let mut due_relay_targets: Vec<(EndpointId, iroh::RelayUrl)> = Vec::new();

        // When this is the only hint we have, it is the lookup's sole route to
        // the peer — we never evict it while partitioned (see the throttled
        // branch below). Step 1 already returned if connected, so reaching here
        // means `alive_count == 0`.
        let is_sole_hint = self.peer_relays.len() == 1;

        // How many hints are reachable from OFF the local network (a public/domain
        // relay or a globally-routable IP). When exactly one is, that hint is the
        // last off-LAN lifeline and must not be evicted while throttled — LAN-only
        // hints are no alternative once the laptop leaves the LAN. See the
        // throttled branch below.
        let offlan_reachable_count = self
            .peer_relays
            .iter()
            .filter(|h| relay_is_offlan_reachable(&h.relay_url))
            .count();

        // Decide per hint, applying node calls (set/remove) and stamping the
        // in-memory `last_attempt_ms` for due hints as we go.
        for hint in self.peer_relays.iter_mut() {
            let Ok(endpoint_id) = hint.endpoint_id.parse::<EndpointId>() else {
                // Malformed snapshot entry — skip silently; startup already warned
                // about this entry at load time.
                continue;
            };

            if hint_attempt_due(hint, now) {
                // Re-add the hint and dial it. Re-add is UNCONDITIONAL of
                // failure_count — the last-hint guarantee that a returning
                // off-LAN peer is never stranded.
                if let Ok(relay_url) = hint.relay_url.parse::<iroh::RelayUrl>() {
                    self.sync_node.set_peer_relay(endpoint_id, &relay_url);
                    // Also drive a supervisor-issued connect for this hint (below):
                    // gossip's bare-id Dialer can park forever and is then never
                    // re-dialed, so we establish the relay-carrying connection
                    // ourselves and hand it to gossip.
                    due_relay_targets.push((endpoint_id, relay_url));
                }
                // Even with an unparseable URL, still bootstrap by EndpointId —
                // mDNS or a prior direct address may reach the peer on-LAN.
                hint.last_attempt_ms = Some(now);
                bootstrap_ids.push(endpoint_id);
                due_ids.push(endpoint_id);
            } else {
                // Throttled. Evict from the lookup UNLESS this hint is a lifeline
                // we must never drop while partitioned:
                //   - the only hint we have at all (sole-hint rule), OR
                //   - the only OFF-LAN-reachable hint — its loss strands us
                //     off-LAN even though LAN-only alternatives remain, because
                //     those LAN-only hints cannot be dialed off-LAN.
                // The off-LAN rule is a strict SUPERSET of the sole-hint rule: a
                // sole LAN-only hint is still retained by `is_sole_hint` (it is
                // the only address we have, useful or not), so this never regresses
                // the original protection.
                let is_sole_offlan_lifeline =
                    relay_is_offlan_reachable(&hint.relay_url) && offlan_reachable_count == 1;
                if !is_sole_hint && !is_sole_offlan_lifeline {
                    // A live alternative remains (another hint, or another off-LAN
                    // route) — safe to evict this one so nothing re-resolves the
                    // dead relay until it comes due again.
                    self.sync_node.remove_peer_relay(endpoint_id);
                }
                // Otherwise RETAINED in the lookup — the relay-reap reconnect fix,
                // generalized to the off-LAN lifeline.
                //
                // We throttle the dial FREQUENCY (the `hint_attempt_due` gate
                // above already does this), not the address PRESENCE — never
                // evicting a partitioned peer's only lifeline. If we removed it,
                // the next due tick would re-seed an address that had been gone,
                // but more importantly a sole reaped relay actor would never be
                // driven back to life between due windows. The off-LAN case adds:
                // a laptop that paired with both a public-relay peer and a LAN-IP
                // peer must keep the public hint resident, because off-LAN the
                // LAN-IP hint is a dead end and the public one is the only route
                // home (this is the n0-DNS-removal regression — n0's parallel DNS
                // resolver used to mask this eviction window).
                //
                // Tradeoff (intentional, and it diverges from `remove_peer_relay`'s
                // own docstring warning): retaining such a hint means a
                // permanently-dead peer's address lingers in `MemoryLookup`, where
                // iroh-gossip's HyParView maintenance re-resolves it on its own ~60s
                // cadence — low-grade relay-actor activity that the general eviction
                // path otherwise quiets between due windows. We accept that churn
                // because the alternative (evicting the only off-LAN address while
                // partitioned) is what caused the 12+ minute non-recovery this fix
                // exists to kill. `MAX_HINT_BACKOFF_MS` still bounds how often WE
                // actively dial it, so retention does not reintroduce a hot dial loop.
            }
        }

        if bootstrap_ids.is_empty() {
            return;
        }

        let attempt_count = bootstrap_ids.len();
        if let Err(e) = self.vault_gossip.rejoin_peers(bootstrap_ids).await {
            warn!("Reconnect attempt failed to re-bootstrap gossip: {e}");
        } else {
            info!(
                hints = attempt_count,
                "Reconnect attempt: re-seeded due hints, re-bootstrapping gossip"
            );
        }

        // Supervisor-issued relay-carrying connect for each due hint.
        //
        // The `rejoin_peers` above re-queues a Join, but gossip dials a bootstrap
        // peer by BARE EndpointId, and that dial can park forever in iroh's
        // address resolution (relay-only, off-LAN — no path ever resolves, no
        // timeout). Once parked, the peer sits `Pending` with a non-empty queue
        // and gossip's `queue.is_empty()` guard suppresses every later re-dial, so
        // the peer is unreachable until restart. We break that by establishing the
        // connection ourselves with the relay attached (an `EndpointAddr` carrying
        // the relay resolves immediately — no park) and handing it to gossip, which
        // adopts it exactly as it adopts an inbound accept: `accept_conn` drains the
        // queued Join, flips the peer Active, and the handshake yields NeighborUp.
        // This is independent of gossip's parked Dialer, so it also recovers a peer
        // that is ALREADY stuck.
        for (endpoint_id, relay_url) in due_relay_targets {
            self.spawn_bootstrap_connect(endpoint_id, relay_url);
        }

        // Step 5: every due hint failed to yield a neighbor this tick (step 1
        // returned when connected). Bump the in-memory throttle so the per-hint
        // backoff ramps for the rest of this session.
        self.record_due_hint_failures(&due_ids);
    }

    /// Establish a relay-carrying connection to a due peer and hand it to gossip,
    /// recovering peers whose bare-id gossip bootstrap dial parked (see
    /// `on_reconnect_tick`).
    ///
    /// Spawned, not awaited inline: `endpoint.connect` can take up to the QUIC
    /// handshake timeout to fail when the relay is unreachable, and blocking the
    /// daemon's single event loop that long would stall sync handling and the
    /// net-change channel. The in-flight `HashSet` guard is load-bearing: the
    /// supervisor ticks every ~200ms after a network change, so without it the
    /// same peer would accumulate dozens of concurrent connects. We skip a peer
    /// whose connect is already in flight, insert before spawning, and remove when
    /// the task finishes (both success and failure).
    fn spawn_bootstrap_connect(&self, endpoint_id: EndpointId, relay_url: iroh::RelayUrl) {
        let endpoint = self.sync_node.endpoint.clone();
        let gossip = self.sync_node.gossip.clone();
        let rejoin = self.vault_gossip.rejoin_handle();
        let inflight = self.bootstrap_connect_inflight.clone();

        tokio::spawn(async move {
            // Skip if a connect to this peer is already in flight; otherwise claim
            // the slot. Holding the lock across the insert keeps the check-and-set
            // atomic against concurrent ticks.
            {
                let mut set = inflight.lock().await;
                if !set.insert(endpoint_id) {
                    return;
                }
            }

            // Re-queue a Join so a fresh Join is present in gossip's proto queue
            // for `accept_conn` to drain on adoption (deterministic ordering — the
            // queued Join is what initiates the handshake from our side once the
            // connection is adopted).
            if let Err(e) = rejoin.rejoin_peers(vec![endpoint_id]).await {
                debug!(peer = %endpoint_id, "bootstrap re-join before connect failed: {e}");
            }

            // The relay-carrying addr is inserted as an `App` path before address
            // lookup runs, so the connect resolves immediately and cannot park.
            let addr = EndpointAddr::new(endpoint_id).with_relay_url(relay_url);
            match endpoint.connect(addr, GOSSIP_ALPN).await {
                Ok(conn) => {
                    // Adopt the connection into gossip: drains the queued Join →
                    // peer Active → handshake → NeighborUp.
                    if let Err(e) = gossip.handle_connection(conn).await {
                        debug!(peer = %endpoint_id, "gossip rejected supervisor bootstrap connection: {e}");
                    }
                }
                Err(e) => {
                    // A real, fast failure now (vs the parked bare-id dial) — the
                    // relay is genuinely unreachable this attempt. The next due
                    // tick retries.
                    debug!(peer = %endpoint_id, "supervisor bootstrap connect failed: {e}");
                }
            }

            inflight.lock().await.remove(&endpoint_id);
        });
    }

    /// Connected-path allowlist reconcile, throttled to `roster_reconcile_interval_ms`.
    ///
    /// Called from `on_reconnect_tick`'s connected branch. Re-pushes the
    /// membership roster to live neighbors on a slow cadence so members that
    /// drifted across a restart or a missed `NeighborUp` re-converge even with no
    /// new pairings. Cheap: one small gossip broadcast at most once per window.
    async fn maybe_reconcile_allowlist_roster(&mut self) {
        let now = now_ms();
        let due = match self.last_roster_reconcile_ms {
            None => true,
            Some(last) => now.saturating_sub(last) >= self.roster_reconcile_interval_ms,
        };
        if !due {
            return;
        }
        self.last_roster_reconcile_ms = Some(now);
        self.push_allowlist_roster().await;
    }

    /// Shrink the connected-path roster-reconcile throttle so integration tests
    /// can observe a reconcile within `wait_until`'s budget.
    ///
    /// `pub` solely for integration-test access — same rationale as
    /// [`Daemon::set_reconnect_interval`] (the test binary compiles this crate
    /// without the `test` cfg, so a `#[cfg(test)]` gate would break it). This is a
    /// SEAM on the daemon's own reconcile timer, not a widening of the allowlist
    /// API. Do not call from production code paths.
    pub fn set_roster_reconcile_interval(&mut self, period: std::time::Duration) {
        self.roster_reconcile_interval_ms = period.as_millis() as u64;
    }

    /// Bump `failure_count` for each hint dialed-but-unconnected this tick.
    ///
    /// Updates the in-memory snapshot (drives the next tick's throttle) and
    /// bumps the hint's in-memory `failure_count` and emits the one-shot eviction
    /// log when a hint first crosses [`STALE_FAILURE_THRESHOLD`].
    ///
    /// The throttle is runtime-only: the supervisor's working set is seeded from
    /// the `allowlist × known_public_relays` cross-product on each boot, so a
    /// restart resetting backoff is fine and there is nothing to persist.
    fn record_due_hint_failures(&mut self, due_ids: &[EndpointId]) {
        if due_ids.is_empty() {
            return;
        }

        let due_hex: Vec<String> = due_ids.iter().map(|id| id.to_string()).collect();

        // In-memory bump + one-shot stale-transition log.
        for hint in self.peer_relays.iter_mut() {
            if due_hex.contains(&hint.endpoint_id) {
                hint.failure_count = hint.failure_count.saturating_add(1);
                if hint.failure_count == STALE_FAILURE_THRESHOLD {
                    info!(
                        endpoint_id = %hint.endpoint_id,
                        failure_count = hint.failure_count,
                        backoff_ms = MAX_HINT_BACKOFF_MS,
                        "Peer relay hint stale — evicting from lookup, retrying slowly"
                    );
                }
            }
        }
    }

    /// A network change occurred (wifi switch, hotspot, new LAN). Reset every
    /// peer-relay hint's backoff so the next supervisor tick re-dials immediately
    /// on the new network — the fix for "every net change needs a restart."
    ///
    /// Purely additive: the supervisor's dial/evict/sole-hint logic
    /// (`on_reconnect_tick`) is untouched; we only un-throttle the hints by
    /// zeroing `failure_count`/`last_attempt_ms`, which flips `hint_attempt_due`
    /// to `true` for every hint so they all re-dial on the next wake.
    ///
    /// This pairs with `on_reconnect_tick`'s off-LAN-lifeline retention to protect
    /// the public-relay lifeline across a network switch: retention keeps it
    /// resident in the lookup even if a reconnect tick races ahead of this reset,
    /// and this reset re-dials it immediately on the new network.
    ///
    /// It ALSO kicks the mDNS discovery channel (republish addresses + restart
    /// the browse) so same-LAN re-discovery after a migration is prompt. Note this
    /// is a DIFFERENT channel from iroh's gossip re-announce: iroh re-publishes our
    /// `EndpointAddr` to peers we're ALREADY connected to (see `startup.rs`), while
    /// mDNS republish+re-browse is what re-establishes LAN discovery with peers we
    /// got partitioned from on the move. They don't overlap.
    async fn on_network_change(&mut self) {
        if self.peer_relays.is_empty() {
            return; // unpaired / hint-less — nothing to re-dial.
        }
        info!(
            hints = self.peer_relays.len(),
            "Network change detected — resetting reconnect backoff"
        );

        // (1) In-memory snapshot (the supervisor's source of truth). This working
        // set is runtime-only — re-seeded from the cross-product on each boot — so
        // there is nothing to persist; a restart mid-flap re-seeds fresh anyway.
        for hint in self.peer_relays.iter_mut() {
            hint.failure_count = 0;
            hint.last_attempt_ms = None;
        }

        // (2) Kick mDNS: re-advertise addresses + restart the browse for prompt
        // same-LAN re-discovery. Strictly additive — synchronous, best-effort,
        // and a no-op when mDNS is unavailable.
        self.sync_node.republish_mdns_on_net_change();
    }

    /// Refresh a peer's hint after a successful background sync exchange.
    ///
    /// This is what makes stale-hint eviction safe: a reachable peer's
    /// in-memory hint is continuously reset (success stamped, `failure_count`
    /// zeroed) so it never goes stale, and a fresh relay URL learned mid-session
    /// replaces a moved peer's stale one. Touches three runtime places, kept in
    /// step (the per-peer hint is NOT persisted — `known_public_relays` is the
    /// sole durable networking store; this only refreshes session state):
    ///
    /// 1. **In-memory snapshot** (supervisor's truth): stamp + reset the matching
    ///    hint. If we learned a relay for a peer we had NO hint for, INSERT one —
    ///    this fixes the responder-side asymmetry where only the pairing initiator
    ///    ever seeded the other side's relay.
    /// 2. **Public-relay set** (`learn_public_relay`): a learned relay that is a
    ///    NEW public relay (a second server's home) is adopted into the RelayMap +
    ///    persisted `known_public_relays` for failover. No-op otherwise.
    /// 3. **Live lookup** (`set_peer_relay`): only when a URL is known, so the
    ///    next dial uses the fresh hint.
    async fn on_exchange_learned(&mut self, learned: ExchangeLearned) {
        let endpoint_hex = learned.endpoint_id.to_string();
        let now = now_ms();

        // (1) In-memory snapshot (session-only; re-seeded from the cross-product
        // on the next boot).
        if let Some(hint) = self
            .peer_relays
            .iter_mut()
            .find(|r| r.endpoint_id == endpoint_hex)
        {
            hint.last_success_ms = Some(now);
            hint.failure_count = 0;
            if let Some(ref url) = learned.relay_url {
                hint.relay_url = url.to_string();
            }
        } else if let Some(ref url) = learned.relay_url {
            // No hint yet, but we learned this peer's relay — record it so the
            // supervisor can re-dial them this session (responder-asymmetry fix).
            let mut entry =
                crate::persistence::PeerRelay::new(endpoint_hex.clone(), url.to_string());
            entry.last_success_ms = Some(now);
            self.peer_relays.push(entry);
        }

        // (2 — C6 gossip expansion) The learned relay may also be a NEW public
        // relay (a second server's home relay), in which case we adopt it into our
        // own RelayMap + persisted public-set for failover redundancy. Borrows the
        // relay URL so block (3) below can still move it. No-op for LAN-direct
        // (`None`), private, or already-known relays.
        self.learn_public_relay(learned.relay_url.as_ref()).await;

        // (3) Live re-seed so the next dial uses the fresh hint.
        if let Some(url) = learned.relay_url {
            self.sync_node.set_peer_relay(learned.endpoint_id, &url);
        }

        debug!(endpoint_id = %endpoint_hex, "Refreshed peer relay hint on successful exchange");
    }

    /// Adopt a relay learned from a peer into this node's public-relay set, when it
    /// is a new public relay (C6 gossip expansion).
    ///
    /// A server's home relay IS its public relay, so once a laptop joins the mesh
    /// it can discover public relays it was never paired with (e.g. a second
    /// server's) and adopt them for failover redundancy — live (no restart) and
    /// persisted (so the next cold start homes on them too).
    ///
    /// Three effects, gated together on the adoption decision (so a private,
    /// already-known, or absent relay does nothing):
    /// 1. **Live RelayMap** (`add_home_relay` → `insert_relay`) so net-report can
    ///    probe + fail over to it THIS session.
    /// 2. **Persisted public set** (`add_known_public_relay`) so it survives restart.
    /// 3. **Supervisor cross-product** — a new public relay means new
    ///    `(allowlist peers) × {new relay}` reconnect targets, so each trusted peer
    ///    gains a `(peer, new_relay)` hint (in-memory snapshot + live `peer_lookup`),
    ///    letting the supervisor dial already-trusted peers through it.
    ///
    /// SECURITY: this only ever adds a TRANSPORT hint. The trusted EndpointIds come
    /// from the ALLOWLIST; the relay never contributes or changes an identity. A
    /// learned relay paired with an already-trusted EndpointId is TLS-verified on
    /// dial regardless of the relay, so a hostile relay is DoS/metadata only.
    ///
    /// The idempotency guard reads the persisted `known_public_relays` (iroh exposes
    /// no `Endpoint::contains_relay()`): since every relay we insert we also persist
    /// and vice-versa, set-membership stands in for RelayMap membership and prevents
    /// per-exchange churn. The config read is skipped for the common LAN-direct /
    /// private cases via the cheap classifier pre-check below.
    async fn learn_public_relay(&mut self, learned: Option<&iroh::RelayUrl>) {
        // Cheap pre-filter: skip the disk read for the overwhelmingly common cases
        // (LAN-direct exchange → None; a loopback/LAN relay → private). Only a
        // genuinely public learned relay is worth a config round-trip.
        let candidate = match learned {
            Some(url) if relay_is_offlan_reachable(&url.to_string()) => url,
            _ => return,
        };

        // Load the persisted set to apply the idempotency guard. A read failure is
        // non-fatal: skip this adoption rather than risk churn on an unknown state.
        let known = match crate::persistence::DaemonConfig::load_or_generate(&self.vault_path, None)
            .await
        {
            Ok((config, _)) => config.known_public_relays,
            Err(e) => {
                warn!("Failed to read config for learned-relay adoption: {e}");
                return;
            }
        };
        let Some(url) = learned_public_relay_to_adopt(Some(candidate), &known) else {
            return;
        };
        let url_str = url.to_string();

        // (1) Live RelayMap.
        self.sync_node.add_home_relay(&url).await;

        // (2) Persist into the cold store.
        let persist = crate::persistence::persist_config_change(
            &self.vault_path,
            self.relay_url.clone(),
            |config| config.add_known_public_relay(&url_str),
        )
        .await;
        if let Err(e) = persist {
            warn!("Failed to persist learned public relay: {e}");
        }

        // (3) Refresh the supervisor's cross-product with (allowlist peers) × {new
        // relay}. Mirrors the startup seed in `startup.rs`: skip self (iroh rejects
        // self-directed relay paths) and invalid EndpointIds; dedup against the
        // existing snapshot so re-learning is a no-op.
        let own_endpoint_id = self.sync_node.node_id();
        let peers = match self.allowlist.list_peers().await {
            Ok(peers) => peers,
            Err(e) => {
                warn!("Failed to read allowlist for learned-relay cross-product: {e}");
                return;
            }
        };
        for peer in &peers {
            let endpoint_id = match EndpointId::from_bytes(peer.node_id.as_bytes()) {
                Ok(id) => id,
                Err(e) => {
                    warn!("Skipping invalid allowlist peer for learned-relay seed: {e}");
                    continue;
                }
            };
            if endpoint_id == own_endpoint_id {
                continue;
            }
            let endpoint_hex = peer.node_id.to_string();
            let already = self
                .peer_relays
                .iter()
                .any(|h| h.endpoint_id == endpoint_hex && h.relay_url == url_str);
            if already {
                continue;
            }
            self.sync_node.add_peer_relay(endpoint_id, &url);
            self.peer_relays.push(crate::persistence::PeerRelay::new(
                endpoint_hex,
                url_str.clone(),
            ));
        }
    }

    /// Shrink the reconnect-supervisor tick interval so integration tests can
    /// observe recovery within `wait_until`'s 10s budget.
    ///
    /// This method is `pub` solely for integration test access — call it before
    /// `run_loop` starts. Production uses the [`RECONNECT_BASE_MS`] cadence built
    /// in `Daemon::new`. It cannot be `#[cfg(test)]`: integration-test binaries
    /// compile this crate without the `test` cfg, so the gate would break them.
    /// Do not call from production code paths.
    pub fn set_reconnect_interval(&mut self, period: std::time::Duration) {
        let mut tick = tokio::time::interval(period);
        tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        self.reconnect_tick = tick;
    }

    /// Wire the network-change signal that resets reconnect backoff.
    ///
    /// Production passes a receiver fed by `endpoint.watch_addr()` (see
    /// `startup.rs`); tests pass a receiver whose sender they hold, so `send(())`
    /// simulates a net change with no real network. `pub` for integration-test
    /// access — same seam rationale as [`Daemon::set_reconnect_interval`]
    /// (integration-test binaries compile this crate without the `test` cfg, so a
    /// `#[cfg(test)]` gate would break them). Not a public production API.
    pub fn set_net_change_rx(&mut self, rx: mpsc::Receiver<()>) {
        self.net_change_rx = Some(rx);
    }

    /// Wire the inbound-sync freshness receiver — the other half of the channel
    /// the pumped inbound handler sends on (built alongside the `SyncNode` in
    /// `startup.rs` / `build_node`). The run-loop drains it to stamp
    /// `peer_registry.update_last_seen` for peers that synced inbound, so a peer
    /// that only ever connects inbound still counts as alive (S2). `pub` for
    /// integration-test access — same seam rationale as [`Daemon::set_net_change_rx`].
    pub fn set_inbound_seen_rx(&mut self, rx: mpsc::UnboundedReceiver<PeerId>) {
        self.inbound_seen_rx = Some(rx);
    }

    /// Populate the in-memory peer-relay snapshot the supervisor re-dials from.
    ///
    /// Called once at startup with the `allowlist × known_public_relays`
    /// cross-product (built in `startup.rs` — the same set that seeds the live
    /// lookup), and refreshed at runtime by pairing / learn-on-exchange. Held in
    /// memory rather than reloaded from disk per tick: the per-peer hint is no
    /// longer persisted (`known_public_relays` is the sole durable store), and the
    /// integration harness — which runs against a non-existent vault path — can
    /// seed it directly.
    ///
    /// Asymmetry (inherited from pairing, see `pair_shared.rs`): only the
    /// INITIATOR learns the responder's relay at pair time, so after a fresh pair
    /// only the initiator's snapshot gains a direct entry — the responder reaches
    /// the initiator via the cross-product (the initiator's public relay is in
    /// `known_public_relays`) or once a learn-on-exchange follow-up fires.
    ///
    /// In `MemoryLookup` test wiring the actual dial resolves via direct
    /// addresses, so a seeded relay URL only needs to parse, not point anywhere
    /// real.
    pub fn seed_peer_relays_snapshot(&mut self, peer_relays: Vec<crate::persistence::PeerRelay>) {
        self.peer_relays = peer_relays;
    }

    /// Drive the C6 learned-public-relay adoption path with a fabricated learned
    /// relay, for integration-test access.
    ///
    /// A genuine relay-routed exchange between in-process nodes learns a loopback
    /// URL, which the off-LAN-reachable classifier (correctly) rejects — so the
    /// positive adoption path (a public relay a real second server advertises) is
    /// only reachable in tests by injecting the URL. Exercises the real
    /// `learn_public_relay` logic; the production loop reaches it via
    /// `on_exchange_learned`. `pub` for the same seam rationale as
    /// [`Daemon::seed_peer_relays_snapshot`] — integration-test binaries compile
    /// this crate without the `test` cfg, so a `#[cfg(test)]` gate would break them.
    /// Not a public production API.
    pub async fn learn_public_relay_for_test(&mut self, relay_url: &iroh::RelayUrl) {
        self.learn_public_relay(Some(relay_url)).await;
    }

    /// Read-only view of the supervisor's in-memory peer-relay snapshot, for
    /// integration-test assertions (e.g. that a learned public relay seeded the
    /// cross-product). `pub` for the same seam rationale as the methods above.
    pub fn peer_relays_snapshot_for_test(&self) -> &[crate::persistence::PeerRelay] {
        &self.peer_relays
    }

    /// Drive the real learn-on-exchange refresh with a fabricated exchange
    /// outcome, for integration-test access.
    ///
    /// Exercises the production [`Daemon::on_exchange_learned`] handler — the
    /// path a genuine NeighborUp sync reaches via the run loop's
    /// `exchange_learn_rx` arm. A test owns the daemon and drives this directly so
    /// it can then assert the in-memory reset via [`Daemon::peer_relays_snapshot_for_test`]
    /// (the supervisor's working set lives inside the daemon, so it can't be read
    /// from a daemon that has been moved into a spawned `run_loop` task). `pub` for
    /// the same seam rationale as [`Daemon::seed_peer_relays_snapshot`] —
    /// integration-test binaries compile this crate without the `test` cfg, so a
    /// `#[cfg(test)]` gate would break them. Not a public production API.
    pub async fn apply_exchange_success_for_test(
        &mut self,
        endpoint_id: EndpointId,
        relay_url: Option<iroh::RelayUrl>,
    ) {
        self.on_exchange_learned(ExchangeLearned {
            endpoint_id,
            relay_url,
        })
        .await;
    }

    /// Record a single learned/refreshed peer relay into the in-memory snapshot
    /// (last-write-wins per endpoint_id), keeping the supervisor's view current
    /// after a runtime pairing without a restart.
    pub fn upsert_peer_relay_snapshot(&mut self, endpoint_id: String, relay_url: String) {
        if let Some(existing) = self
            .peer_relays
            .iter_mut()
            .find(|r| r.endpoint_id == endpoint_id)
        {
            existing.relay_url = relay_url;
        } else {
            self.peer_relays
                .push(crate::persistence::PeerRelay::new(endpoint_id, relay_url));
        }
    }

    /// Dispatch a `DaemonCommand` from the desktop tray.
    async fn on_daemon_command(&mut self, cmd: DaemonCommand) {
        match cmd {
            DaemonCommand::StartDiscovery { reply } => {
                self.start_initiator_discovery(reply).await;
            }
            DaemonCommand::RequestPairing { vault_id, reply } => {
                self.request_initiator_pairing(vault_id, reply).await;
            }
            DaemonCommand::SubmitCode {
                vault_id,
                code,
                reply,
            } => {
                self.submit_initiator_code(vault_id, code, reply).await;
            }
            DaemonCommand::CancelInitiate { reply } => {
                if let Some(session) = self.active_initiator.take() {
                    session.cancel.cancel();
                    info!("Initiator pairing session cancelled by user");
                }
                let _ = reply.send(());
            }
            DaemonCommand::RejectInbound { reply } => {
                // Drop the active pairing session. This closes reply_tx, which
                // the pairing handler treats as an immediate rejection.
                if self.active_pairing.take().is_some() {
                    info!("Inbound pairing rejected by user");
                    self.emit_pairing_event(PairingUiEvent::InboundFailed {
                        reason: "Rejected by user".to_string(),
                    });
                }
                let _ = reply.send(());
            }
        }
    }

    /// Check whether a peer is allowed to sync.
    ///
    /// Returns `true` only if the peer's `node_id` is in the allowlist.
    ///
    /// Returns `false` if:
    /// - The allowlist is empty (no peers paired yet — deny all until paired), OR
    /// - The peer is not in the allowlist, OR
    /// - The allowlist cannot be read (fail-closed for safety).
    ///
    /// The "open until first pair" behavior was removed because it creates a
    /// window where any device on the network can sync before pairing completes.
    /// Pair first with `memory sync pair`, then start the daemon.
    async fn is_peer_allowed(&self, peer_id: &PeerId) -> bool {
        // Delegate to the trait `is_allowed` so the tombstone filter is the single
        // source of truth — an inline `list_peers().any(...)` scan here would treat
        // a revoked (tombstoned) peer as still trusted, bypassing revocation.
        // Fail closed: any read error denies sync (matches the deny-on-error policy).
        match self.allowlist.is_allowed(peer_id).await {
            Ok(allowed) => allowed,
            Err(e) => {
                error!("Failed to read allowlist, denying sync: {}", e);
                false
            }
        }
    }

    /// Handle a file change event from the watcher, routing through the
    /// move-coalescer so a native rename (delivered as an unlinked delete+create —
    /// see [`crate::move_coalescer`]) collapses into one same-UUID move rather than
    /// a tombstone plus a fresh-UUID create.
    ///
    /// Only events that COULD be half of a move enter the window:
    /// - a `Modified` at a path with a still-live node and NO buffered delete is an
    ///   edit — dispatched immediately, the no-latency common case;
    /// - a `Modified` at a not-yet-tracked path (or a path whose deletion is
    ///   currently buffered — the re-create-at-a-just-deleted-path edge, OQ-D) is a
    ///   create-candidate;
    /// - a `Deleted` is always a move-candidate (its standalone meaning on expiry is
    ///   a real tombstone).
    async fn on_file_changed(&mut self, event: FileEvent) {
        match event.kind {
            FileEventKind::Modified => {
                // An in-place edit of an already-tracked file is NOT a move
                // candidate — dispatch it immediately so the common case adds zero
                // latency. The buffered-delete check keeps OQ-D (a re-create at a
                // path whose deletion we are deliberately holding) on the create
                // path: the node still reads "live" only because the buffer has not
                // tombstoned it yet.
                let is_create = !self.path_has_live_node(&event.path).await
                    || self.move_coalescer.has_pending_delete(&event.path);
                if !is_create {
                    self.on_file_modified(&event.path).await;
                    return;
                }
                self.on_create_candidate(&event.path).await;
            }
            FileEventKind::Deleted => {
                self.on_delete_candidate(&event.path).await;
            }
        }
    }

    /// Whether the index currently has a live node at `path`.
    async fn path_has_live_node(&self, path: &str) -> bool {
        let vault = self.vault.lock().await;
        vault.index().node_for_path(path).is_some()
    }

    /// Feed a create-candidate `Modified` into the coalescer and act on its
    /// decision: pair it into a move now, or buffer it for the window.
    async fn on_create_candidate(&mut self, path: &str) {
        let Some(hash) = self.hash_new_file(path).await else {
            // Couldn't read/parse the new file — fall back to the standalone create
            // path rather than buffering an event we can't pair.
            self.on_file_modified(path).await;
            return;
        };
        match self.move_coalescer.on_create(hash, path) {
            MoveDecision::Move { old_path, new_path } => {
                self.emit_file_move(&old_path, &new_path).await;
            }
            MoveDecision::Buffered => {}
        }
    }

    /// Feed a delete-candidate `Deleted` into the coalescer and act on its
    /// decision: pair it into a move now, or buffer it for the window.
    ///
    /// The doc's content hash is captured from its still-present `ContentDoc`
    /// BEFORE any tombstone (the buffer holds the event; nothing is deleted until
    /// the window resolves).
    async fn on_delete_candidate(&mut self, path: &str) {
        let Some(hash) = self.hash_tracked_doc(path).await else {
            // No content doc resolves for this path (an unknown/already-gone path) —
            // it can't be the old half of a move; commit it standalone.
            self.on_file_deleted(path).await;
            return;
        };
        match self.move_coalescer.on_delete(hash, path) {
            MoveDecision::Move { old_path, new_path } => {
                self.emit_file_move(&old_path, &new_path).await;
            }
            MoveDecision::Buffered => {}
        }
    }

    /// Content hash of a tracked document at `path`, in the same domain as
    /// [`Self::hash_new_file`] (both hash the canonical materialized markdown).
    /// `None` if no node resolves for the path.
    async fn hash_tracked_doc(&self, path: &str) -> Option<[u8; 32]> {
        let vault = self.vault.lock().await;
        vault.index().node_for_path(path)?;
        let doc = vault.get_document(path).await.ok()?;
        Some(vault_sync::content_hash(&doc))
    }

    /// Content hash of a not-yet-tracked `.md` on disk, built from its markdown so
    /// it lands in the same hash domain as the delete side. The author is
    /// irrelevant — [`vault_sync::content_hash`] is taken over the document's
    /// materialized markdown, not its peer id.
    async fn hash_new_file(&self, path: &str) -> Option<[u8; 32]> {
        let vault = self.vault.lock().await;
        let bytes = vault.read_raw(path).await.ok()?;
        let content = String::from_utf8_lossy(&bytes);
        let doc = vault_sync::ContentDoc::from_markdown(&content, 0).ok()?;
        Some(vault_sync::content_hash(&doc))
    }

    /// Execute a matched move: re-parent the existing node (preserving its UUID and
    /// content — `move_node` re-transfers nothing, INV-1), persist the index, and
    /// broadcast the new path so peers apply the structural `tree.mov`.
    ///
    /// A same-path move (the OQ-D re-create) is a no-op in the index — the doc
    /// stays alive under its UUID and no broadcast is needed.
    async fn emit_file_move(&mut self, old_path: &str, new_path: &str) {
        if old_path == new_path {
            info!("Move coalesced to a same-path no-op: {}", old_path);
            return;
        }

        let vault = self.vault.lock().await;
        if let Err(e) = vault.index().move_node(old_path, new_path) {
            error!(
                "Failed to coalesce move {} -> {}: {}",
                old_path, new_path, e
            );
            return;
        }
        if let Err(e) = vault.save_index().await {
            error!(
                "Failed to persist index after move {} -> {}: {}",
                old_path, new_path, e
            );
            return;
        }
        drop(vault);

        info!("Coalesced native move: {} -> {}", old_path, new_path);

        let has_peers = self.peer_registry.lock().await.alive_count() > 0;
        if has_peers && let Err(e) = self.vault_gossip.broadcast_change(new_path).await {
            error!("Failed to broadcast move of {}: {}", new_path, e);
        }
    }

    /// Sweep the coalescer for events whose window expired with no partner and
    /// commit each to its standalone meaning. Driven by `move_sweep_tick`.
    async fn sweep_pending_moves(&mut self) {
        let expired = self.move_coalescer.sweep();
        self.commit_expired(expired).await;
    }

    /// Dispatch each expired record to its standalone sink (the same handlers the
    /// immediate-dispatch path used before coalescing).
    async fn commit_expired(&mut self, expired: Vec<Expired>) {
        for record in expired {
            match record {
                Expired::StandaloneDelete { path } => {
                    self.on_file_deleted(&path).await;
                }
                Expired::StandaloneCreate { path } => {
                    self.on_file_modified(&path).await;
                }
            }
        }
    }

    /// Handle a file deletion — update vault state then notify peers.
    ///
    /// The sync-flag early-return was removed: `delete_file` is idempotent (returns
    /// false for an already-absent path) so we always apply the event and gate the
    /// broadcast on whether a live node was actually tombstoned.
    async fn on_file_deleted(&mut self, path: &str) {
        info!("File deleted: {}", path);

        let vault = self.vault.lock().await;

        let deleted = match vault.delete_file(path).await {
            Ok(v) => v,
            Err(e) => {
                error!("Failed to delete file {}: {}", path, e);
                return;
            }
        };

        drop(vault);

        if !deleted {
            return;
        }

        if self.peer_registry.lock().await.alive_count() > 0 {
            if let Err(e) = self.vault_gossip.broadcast_change(path).await {
                error!("Failed to broadcast deletion of {}: {}", path, e);
            } else {
                info!(
                    "Broadcast deletion of {} to {} peer(s)",
                    path,
                    self.peer_registry.lock().await.alive_count()
                );
            }
        } else {
            info!(
                "Deleted {} from registry tree (no peers to broadcast)",
                path
            );
        }
    }

    /// Handle a file modification — update vault state then notify peers.
    ///
    /// The sync-flag early-return was removed: `on_file_changed` is echo-safe
    /// (diff-and-merge returns false when content is unchanged) so we always apply
    /// the event and gate the broadcast on whether the doc actually changed.
    async fn on_file_modified(&mut self, path: &str) {
        let vault = self.vault.lock().await;

        // Always update vault state so .loro files stay current for future sync exchanges.
        // Without this, edits made while no peers are connected would be silently lost
        // when a peer connects — prepare_request trusts .loro state, with no fallback.
        let changed = match vault.on_file_changed(path).await {
            Ok(v) => v,
            Err(e) => {
                error!("Failed to process file change for {}: {}", path, e);
                return;
            }
        };

        // Persist any new registry registration produced by on_file_changed.
        // on_file_changed is sync-agnostic (called from both the watcher and reconcile
        // paths); the caller owns the flush decision. Reconcile batches to one save;
        // watcher events each flush here so a restart doesn't lose newly-created file nodes.
        if let Err(e) = vault.save_index().await {
            error!(
                "Failed to persist registry after file change for {}: {}",
                path, e
            );
            return;
        }

        drop(vault);

        if !changed || self.peer_registry.lock().await.alive_count() == 0 {
            return;
        }

        // Notify peers via gossip; they will open a QUIC stream to pull the full update.
        if let Err(e) = self.vault_gossip.broadcast_change(path).await {
            error!("Failed to broadcast change for {}: {}", path, e);
        } else {
            info!(
                "Broadcast change for {} to {} peer(s)",
                path,
                self.peer_registry.lock().await.alive_count()
            );
        }
    }

    /// A peer joined the gossip swarm — initiate a full sync via QUIC.
    ///
    /// The allowlist check and sync-request preparation happen synchronously so
    /// the peer registry is updated before the event loop moves on. The QUIC
    /// exchange and response processing are spawned as a background task so the
    /// event loop remains free to handle inbound sync requests from the same peer
    /// — both sides fire NeighborUp simultaneously and each tries to connect to
    /// the other, which would deadlock if the QUIC call blocked the select loop.
    async fn on_neighbor_up(&mut self, node_id: EndpointId) {
        info!(peer = %node_id, "Gossip NeighborUp — initiating full sync");
        let peer_id = PeerId::from_bytes(*node_id.as_bytes());
        self.peer_registry
            .lock()
            .await
            .on_neighbor_up(peer_id.clone());
        self.emit_status().await;

        if !self.is_peer_allowed(&peer_id).await {
            warn!(peer = %node_id, "Peer not in allowlist, skipping sync");
            return;
        }

        let vault = self.vault.lock().await;
        let request_bytes = match vault.prepare_request().await {
            Ok(bytes) => bytes,
            Err(e) => {
                error!("Failed to prepare sync request for {}: {}", node_id, e);
                return;
            }
        };
        drop(vault);

        // Spawn the QUIC exchange as a background task so the event loop can
        // continue processing inbound sync requests. When both peers fire
        // NeighborUp simultaneously, each needs to respond to the other's
        // inbound sync — which can only happen if the event loop isn't blocked.
        spawn_sync_exchange(
            node_id,
            peer_id,
            request_bytes,
            self.vault.clone(),
            self.allowlist.clone(),
            self.peer_registry.clone(),
            self.sync_node.endpoint.clone(),
            self.exchange_learn_tx.clone(),
            SyncExchangeKind::NeighborUp,
        );

        // Push our full membership roster to the swarm so the peer that just came
        // up converges to it — the eventually-consistent fix for the one-shot
        // lossy `AllowlistUpdate` delta (a peer offline at a pairing's broadcast
        // instant would otherwise never learn the rest of the mesh). The newly
        // joined peer is a current neighbor and will receive it; re-sending to
        // peers that already have the entries is safe (merge_roster is idempotent).
        self.push_allowlist_roster().await;
    }

    /// A peer left the gossip swarm.
    async fn on_neighbor_down(&mut self, node_id: EndpointId) {
        info!(peer = %node_id, "Gossip NeighborDown");
        let peer_id = PeerId::from_bytes(*node_id.as_bytes());
        self.peer_registry.lock().await.on_neighbor_down(&peer_id);
        self.emit_status().await;
    }

    /// A change notification arrived via gossip — pull the changed file via QUIC.
    ///
    /// The allowlist check happens synchronously. The QUIC exchange is spawned as
    /// a background task so the event loop stays free to process other events while
    /// the pull is in flight.
    async fn on_change_received(&mut self, from: EndpointId, path: String) {
        debug!(peer = %from, path = %path, "Change notification received — pulling update");
        let peer_id = PeerId::from_bytes(*from.as_bytes());

        if !self.is_peer_allowed(&peer_id).await {
            warn!(peer = %from, "Change notification from non-allowlisted peer, ignoring");
            return;
        }

        let vault = self.vault.lock().await;
        let request_bytes = match vault.prepare_request().await {
            Ok(bytes) => bytes,
            Err(e) => {
                error!("Failed to prepare sync request for change pull: {}", e);
                return;
            }
        };
        drop(vault);

        spawn_sync_exchange(
            from,
            peer_id,
            request_bytes,
            self.vault.clone(),
            self.allowlist.clone(),
            self.peer_registry.clone(),
            self.sync_node.endpoint.clone(),
            self.exchange_learn_tx.clone(),
            SyncExchangeKind::ChangePull { path },
        );
    }

    /// Broadcast our full membership roster to the gossip swarm.
    ///
    /// This is the eventually-consistent convergence path that replaces the
    /// one-shot, lossy `AllowlistUpdate` delta: any peer that missed a pairing's
    /// single broadcast learns the whole roster on the next NeighborUp/reconcile.
    /// The receiver's `merge_roster` is a union with tombstone-precedence, so
    /// re-sending to peers that already have the entries is safe and idempotent.
    ///
    /// Bounded fan-out (spec rule 4): a `GossipMessage::AllowlistRoster` rides the
    /// same 1KB-capped `broadcast_message` path as every other gossip envelope.
    /// We PRE-CHECK the encoded size against the cap rather than relying on the
    /// `Err` from `broadcast_message` (a returned `Err` there is a BUG, not the
    /// expected over-cap path). When the roster would exceed the cap we fall back
    /// to chunked per-peer `AllowlistUpdate` deltas — the common 3-machine path
    /// stays a single message and a large mesh degrades safely instead of erroring.
    async fn push_allowlist_roster(&mut self) {
        // Include tombstones — filtering to live-only breaks revocation
        // propagation (a removed peer would never converge to "removed" elsewhere).
        let roster = match self.allowlist.list_peers().await {
            Ok(peers) => peers,
            Err(e) => {
                warn!("Failed to read allowlist for roster push: {}", e);
                return;
            }
        };
        if roster.is_empty() {
            return;
        }

        // Pre-check the encoded envelope size against the gossip cap. Build the
        // same `GossipMessage::AllowlistRoster` the broadcast will, so the size we
        // measure matches what `broadcast_message` would frame.
        let encoded_len = bincode::serialize(&GossipMessage::AllowlistRoster(roster.clone()))
            .map(|b| b.len())
            .unwrap_or(usize::MAX);

        if encoded_len > MAX_GOSSIP_MESSAGE_SIZE {
            warn!(
                roster_len = roster.len(),
                encoded_len,
                cap = MAX_GOSSIP_MESSAGE_SIZE,
                "Allowlist roster exceeds gossip size cap — falling back to per-peer deltas"
            );
            for peer in &roster {
                if let Err(e) = self.vault_gossip.broadcast_allowlist_update(peer).await {
                    warn!(peer_id = %peer.node_id, "Failed to broadcast allowlist delta: {}", e);
                }
            }
            return;
        }

        if let Err(e) = self.vault_gossip.broadcast_allowlist_roster(&roster).await {
            warn!("Failed to broadcast allowlist roster: {}", e);
        }
    }

    /// Handle a full membership roster received via gossip from a mesh member.
    ///
    /// Same sender-trust gate as `on_allowlist_update_received` — we only merge a
    /// roster from a peer already in our allowlist. `merge_roster` unions the
    /// incoming entries with tombstone-precedence, so this both adds peers we were
    /// missing and honors revocations a trusted member propagated.
    async fn on_allowlist_roster_received(&self, from: iroh::EndpointId, peers: Vec<AllowedPeer>) {
        let sender_id = PeerId::from_bytes(*from.as_bytes());
        if !self.is_peer_allowed(&sender_id).await {
            warn!(peer = %from, "Allowlist roster from non-allowlisted peer, ignoring");
            return;
        }

        match self.allowlist.merge_roster(&peers).await {
            Ok(()) => {
                info!(from = %from, count = peers.len(), "Merged allowlist roster via gossip");
            }
            Err(e) => {
                error!("Failed to merge allowlist roster from {}: {}", from, e);
            }
        }
    }

    /// Handle an allowlist update received via gossip from a mesh member.
    ///
    /// Only processes the update if the sender is already in our allowlist —
    /// we don't trust gossip from peers we haven't explicitly paired with.
    async fn on_allowlist_update_received(&self, from: iroh::EndpointId, peer: AllowedPeer) {
        let sender_id = PeerId::from_bytes(*from.as_bytes());
        if !self.is_peer_allowed(&sender_id).await {
            warn!(peer = %from, "Allowlist update from non-allowlisted peer, ignoring");
            return;
        }

        match self
            .allowlist
            .add_peer(peer.node_id, &peer.device_name)
            .await
        {
            Ok(()) => {
                info!(peer_id = %peer.node_id, device = %peer.device_name, "Added peer via gossip allowlist update");
            }
            Err(e) => {
                error!(
                    "Failed to add peer {} from allowlist update: {}",
                    peer.node_id, e
                );
            }
        }
    }

    /// Handle an inbound pairing request from a new device.
    ///
    /// Generates a 6-digit code and logs it so the user can relay it to the new
    /// device. Only one pairing session at a time — concurrent requests are dropped.
    async fn on_inbound_pairing_request(&mut self, exchange: InboundPairingExchange) {
        // Reject concurrent pairing attempts — only one session at a time.
        if let Some(ref existing) = self.active_pairing {
            if !existing.is_expired() {
                warn!(
                    device = %exchange.hello.device_name,
                    "Pairing request rejected: session already active"
                );
                // Dropping reply_tx signals rejection to the handler.
                drop(exchange.reply_tx);
                return;
            }
        }

        let session = PairingSession::new(exchange.remote_id, &exchange.hello.device_name);

        info!(
            "Device '{}' wants to join. Pairing code: {} (expires in 5:00)",
            exchange.hello.device_name, session.code
        );

        let challenge = PairingChallenge {
            node_id: PeerId::from_bytes(*self.sync_node.node_id().as_bytes()),
            device_name: self.device_name.clone(),
        };

        // Collect vault gossip topic bytes for the new device.
        let vault_topic = *self.vault_gossip.topic.as_bytes();

        // Include our relay URL if the embedded relay is running.
        let relay_urls: Vec<String> = self.relay_url.iter().cloned().collect();

        // Include all alive peers plus self as mesh members.
        let mut mesh_members: Vec<PeerId> = self
            .peer_registry
            .lock()
            .await
            .get_alive_peers()
            .iter()
            .map(|e| e.node_id)
            .collect();
        mesh_members.push(PeerId::from_bytes(*self.sync_node.node_id().as_bytes()));

        let approval = PairingApproval {
            code: session.code.clone(),
            challenge,
            vault_topic,
            relay_urls,
            mesh_members,
        };

        // Pairing sessions expire after 5 minutes, matching PAIRING_TIMEOUT_SECS in sync-core.
        let expires_at_ms = now_ms() + (5 * 60 * 1000);

        self.active_pairing = Some(session);

        // Ignore send error — the handler may have timed out or been dropped.
        let _ = exchange.reply_tx.send(approval);

        // Notify the desktop tray so it can open the responder window.
        self.emit_pairing_event(PairingUiEvent::InboundRequest {
            device_name: exchange.hello.device_name.clone(),
            code: self
                .active_pairing
                .as_ref()
                .map(|s| s.code.clone())
                .unwrap_or_default(),
            expires_at_ms,
        });
    }

    /// Handle successful pairing — add the new peer to the allowlist and propagate.
    async fn on_pairing_completed(&mut self, peer_id: PeerId, device_name: String) {
        // Verify the completing peer matches the active session. A mismatch would indicate
        // a race between two concurrent pairing attempts (which we reject at the request
        // stage) or a stale event from a previous session — both should be dropped.
        if self.active_pairing.as_ref().map(|s| &s.remote_id) != Some(&peer_id) {
            warn!(
                peer_id = %peer_id,
                "Pairing completed event does not match active session, ignoring"
            );
            return;
        }

        self.active_pairing = None;

        let allowed_peer = AllowedPeer::new(peer_id.clone(), device_name.clone());

        // Bootstrap: if this is the first pairing, also add ourselves to the allowlist.
        let is_first_pair =
            matches!(self.allowlist.list_peers().await, Ok(peers) if peers.is_empty());

        if let Err(e) = self.allowlist.add_peer(peer_id.clone(), &device_name).await {
            error!("Failed to add paired peer to allowlist: {}", e);
            return;
        }

        if is_first_pair {
            let self_id = PeerId::from_bytes(*self.sync_node.node_id().as_bytes());
            if let Err(e) = self.allowlist.add_peer(self_id, &self.device_name).await {
                warn!("Failed to add self to allowlist on first pair: {}", e);
            }
        }

        // Propagate the new peer to existing mesh members via gossip.
        if let Err(e) = self
            .vault_gossip
            .broadcast_allowlist_update(&allowed_peer)
            .await
        {
            warn!(
                "Failed to broadcast allowlist update for {}: {}",
                device_name, e
            );
        }

        info!("Device '{}' joined the mesh", device_name);

        self.emit_pairing_event(PairingUiEvent::InboundCompleted {
            device_name: device_name.clone(),
        });
        self.emit_status().await;
    }

    /// Handle a failed pairing attempt.
    fn on_pairing_failed(&mut self, peer_id: PeerId, reason: String) {
        // Notify the tray before clearing the session so the responder window can
        // auto-close. This covers timeout, bad-code, and gossip-layer errors — the
        // only path not covered here is an explicit user reject (handled in
        // on_daemon_command via DaemonCommand::RejectInbound, which also calls
        // emit_pairing_event before taking active_pairing).
        self.emit_pairing_event(PairingUiEvent::InboundFailed {
            reason: reason.clone(),
        });
        // Clear the active session so a new pairing attempt can start fresh. This means
        // the user must re-initiate pairing (triggering a new code) rather than retrying
        // with the same code. Keeping the session alive for retry would require matching
        // new QUIC connections to existing sessions, which is more complex. For v1 this
        // tradeoff is acceptable.
        self.active_pairing = None;
        warn!("Pairing failed for {}: {}", peer_id, reason);
    }
}

#[cfg(test)]
mod backoff_tests {
    use super::{HINT_BACKOFF_BASE_MS, MAX_HINT_BACKOFF_MS, hint_attempt_due, per_hint_backoff};
    use crate::persistence::PeerRelay;

    // A failure count comfortably past the shift ceiling, to prove the clamp.
    const STALE_OR_MORE: u32 = 10;

    /// Build a synthetic hint carrying just the freshness state the pure
    /// functions read (the endpoint_id/relay_url are irrelevant here).
    fn hint(failure_count: u32, last_attempt_ms: Option<u64>) -> PeerRelay {
        let mut h = PeerRelay::new("x".repeat(64), "http://relay:3340/".to_string());
        h.failure_count = failure_count;
        h.last_attempt_ms = last_attempt_ms;
        h
    }

    /// Per-hint backoff doubles per failure, saturates at the cap, never overflows.
    #[test]
    fn per_hint_backoff_grows_and_caps() {
        // Zero failures → the base interval.
        assert_eq!(per_hint_backoff(0), HINT_BACKOFF_BASE_MS);
        // Each failure doubles the window.
        assert_eq!(per_hint_backoff(1), HINT_BACKOFF_BASE_MS * 2);
        assert_eq!(per_hint_backoff(2), HINT_BACKOFF_BASE_MS * 4);

        // Climbs to and saturates at the cap; a huge failure count cannot
        // overflow the shift (it's clamped to the ceiling) nor exceed the cap.
        assert_eq!(per_hint_backoff(STALE_OR_MORE), MAX_HINT_BACKOFF_MS);
        assert_eq!(per_hint_backoff(u32::MAX), MAX_HINT_BACKOFF_MS);
        for fc in 0..50 {
            assert!(
                per_hint_backoff(fc) <= MAX_HINT_BACKOFF_MS,
                "backoff must never exceed the cap"
            );
        }
    }

    /// A never-attempted hint is due immediately.
    #[test]
    fn never_attempted_hint_is_due() {
        assert!(hint_attempt_due(&hint(0, None), 0));
        assert!(hint_attempt_due(&hint(9, None), 5_000_000));
    }

    /// A fresh hint (no failures) is due on the base cadence: not due before the
    /// base window elapses, due once it does.
    #[test]
    fn fresh_hint_due_on_base_cadence() {
        let h = hint(0, Some(1_000));
        // Just before the base window: not due.
        assert!(!hint_attempt_due(&h, 1_000 + HINT_BACKOFF_BASE_MS - 1));
        // Exactly at the window: due.
        assert!(hint_attempt_due(&h, 1_000 + HINT_BACKOFF_BASE_MS));
    }

    /// A throttled hint stays not-due inside its (longer) window and becomes due
    /// after it — the failure_count widens the window.
    #[test]
    fn throttled_hint_respects_wider_window() {
        let h = hint(2, Some(10_000));
        let window = per_hint_backoff(2);
        assert!(!hint_attempt_due(&h, 10_000 + window - 1));
        assert!(hint_attempt_due(&h, 10_000 + window));
    }

    /// Last-hint guarantee: even the stalest hint comes due within the capped
    /// window, so a returning off-LAN peer is never permanently abandoned.
    #[test]
    fn stalest_hint_eventually_due() {
        let h = hint(u32::MAX, Some(0));
        // One nanosecond before the cap elapses: still throttled.
        assert!(!hint_attempt_due(&h, MAX_HINT_BACKOFF_MS - 1));
        // At the cap: due again — re-added and dialed.
        assert!(hint_attempt_due(&h, MAX_HINT_BACKOFF_MS));
    }

    /// Clock-backward tolerance: a `last_attempt_ms` in the future (clock jumped
    /// back) yields `0` elapsed via saturating_sub, so the hint simply reads
    /// not-yet-due rather than panicking or wedging. It comes due again once the
    /// clock catches up — self-recovering within the re-add window.
    #[test]
    fn clock_backward_does_not_panic_or_wedge() {
        let h = hint(0, Some(1_000_000));
        // "Now" is before the recorded attempt — not due, no panic.
        assert!(!hint_attempt_due(&h, 0));
        // Once the clock advances past attempt + window, it's due again.
        assert!(hint_attempt_due(&h, 1_000_000 + HINT_BACKOFF_BASE_MS));
    }
}

/// Tests for the C6 gossip-expansion decision: which learned relays a node
/// should adopt into its public-relay set (RelayMap + `known_public_relays`).
#[cfg(test)]
mod learn_public_relay_tests {
    use super::learned_public_relay_to_adopt;

    fn url(s: &str) -> iroh::RelayUrl {
        s.parse().expect("test relay URL should parse")
    }

    /// A peer's home relay that is off-LAN-reachable (a public server relay) and
    /// not yet in our set is adopted — this is how a laptop discovers a second
    /// server's relay from the mesh for failover redundancy.
    #[test]
    fn adopts_new_public_relay() {
        let learned = url("https://server2.example.com/");
        let known: Vec<String> = vec!["https://umbra.computer/".to_string()];
        assert_eq!(
            learned_public_relay_to_adopt(Some(&learned), &known),
            Some(learned.clone())
        );
    }

    /// A LAN-direct exchange carries no relay path (`relay_url = None`) — there is
    /// nothing to adopt.
    #[test]
    fn lan_direct_exchange_adopts_nothing() {
        let known: Vec<String> = vec![];
        assert_eq!(learned_public_relay_to_adopt(None, &known), None);
    }

    /// A learned private LAN-IP relay is never adopted into the public set: it is
    /// useless to any peer that isn't on that LAN, so it must not become a home
    /// candidate. Mirrors the `add_known_public_relay` classifier guard.
    #[test]
    fn private_lan_relay_is_not_adopted() {
        let learned = url("http://192.168.68.52:3340/");
        let known: Vec<String> = vec![];
        assert_eq!(learned_public_relay_to_adopt(Some(&learned), &known), None);
    }

    /// A relay we already track is not re-adopted — this is the idempotency guard
    /// that prevents per-exchange RelayMap churn and duplicate cross-product
    /// reconnect entries.
    #[test]
    fn already_known_relay_is_not_re_adopted() {
        let learned = url("https://umbra.computer/");
        let known: Vec<String> = vec!["https://umbra.computer/".to_string()];
        assert_eq!(learned_public_relay_to_adopt(Some(&learned), &known), None);
    }
}
