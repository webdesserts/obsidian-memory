//! Daemon event loop: the `Daemon` struct, its fields, and all event handlers.
//!
//! Submodule layout:
//! - `startup` — startup seam (`StartupBundle`, `startup_inner`) and public entry points
//!   (`run_with_shutdown_controlled`, `run_with_shutdown`, `run`).
//! - `initiator` — two-step GUI pairing state machine (`InitiatorSession`,
//!   `InitiatorPairOutcome`, `run_initiator_pairing_parked`, and the five
//!   `impl Daemon` initiator methods).

mod startup;
mod initiator;
mod sync_exchange;

// Re-export public names so `sync_daemon::daemon::X` paths stay byte-identical.
pub use startup::{run, run_with_shutdown, run_with_shutdown_controlled};
use initiator::{InitiatorPairOutcome, InitiatorSession};
use sync_exchange::{SyncExchangeKind, spawn_sync_exchange};

use crate::pair_api::{
    ConnectionState, DaemonCommand, DaemonStatus, PairingUiEvent, PeerSummary,
};
use crate::watcher::{FileEvent, FileEventKind};
use iroh::EndpointId;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::{broadcast, mpsc, watch};
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use sync_core::allowlist::{AllowedPeer, AllowlistStorage};
use sync_core::fs::FileSystem;
use sync_core::network::{
    SyncNode,
    gossip::{GossipEvent, VaultGossip},
    pairing::{InboundPairingExchange, PairingApproval, PairingEvent},
    streams::InboundSyncRequest,
};
use sync_core::pairing::{PairingChallenge, PairingSession};
use sync_core::{PeerId, PeerRegistry, Vault};

pub(super) fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
}

/// First reconnect attempt fires ~5s after a daemon drops to zero neighbors —
/// fast enough that a brief flap heals quickly, slow enough to let a transient
/// blip self-resolve before the supervisor acts.
const RECONNECT_BASE_MS: u64 = 5_000;

/// Steady-state partition retry ceiling. With 2× growth from the 5s base, a
/// peer offline for hours costs one re-seed + one `rejoin_peers` per minute —
/// negligible CPU and one log line/minute — while still reconnecting within a
/// minute once the peer returns (vs the pre-supervisor "never").
const RECONNECT_CAP_MS: u64 = 60_000;

/// Advance the reconnect backoff: double, saturating at the cap.
///
/// The supervisor resets to [`RECONNECT_BASE_MS`] whenever it observes a live
/// neighbor; this only grows the interval while partitioned.
fn next_reconnect_backoff(current: u64) -> u64 {
    current.saturating_mul(2).min(RECONNECT_CAP_MS)
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

    // ── reconnect supervisor ────────────────────────────────────────────────
    /// Steady wake cadence for the reconnect supervisor. Backoff decides whether
    /// a given wake actually attempts a re-dial (see `next_attempt_at_ms`).
    reconnect_tick: tokio::time::Interval,
    /// Current reconnect backoff in ms. Starts at [`RECONNECT_BASE_MS`], doubles
    /// (capped at [`RECONNECT_CAP_MS`]) on each partitioned attempt, resets on the
    /// tick that observes a live neighbor.
    reconnect_backoff_ms: u64,
    /// Earliest wall-clock ms for the next reconnect attempt. `0` = attempt now.
    next_attempt_at_ms: u64,
    /// In-memory snapshot of persisted peer relay hints, the supervisor's source
    /// of truth for who to re-dial. Populated at startup from `DaemonConfig` and
    /// updated when pairing learns a peer's relay. Held in memory (not reloaded
    /// from disk per tick) so the test harness — which uses a non-existent vault
    /// path — can seed it directly without writing `daemon.toml`.
    peer_relays: Vec<crate::persistence::PeerRelay>,
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
        // The initiator-outcome channel is internal — created here rather than
        // passed in so the public `Daemon::new` signature (used directly by the
        // test harness) stays unchanged.
        let (initiator_outcome_tx, initiator_outcome_rx) = mpsc::unbounded_channel();

        // The reconnect supervisor wakes on a steady cadence; backoff (tracked in
        // the fields below) gates whether a wake actually re-dials. `Delay` skips
        // missed ticks rather than bursting to catch up after a slow turn.
        let mut reconnect_tick =
            tokio::time::interval(std::time::Duration::from_millis(RECONNECT_BASE_MS));
        reconnect_tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

        Self {
            vault,
            sync_node,
            vault_gossip,
            file_event_rx,
            discovery_rx,
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
            reconnect_tick,
            reconnect_backoff_ms: RECONNECT_BASE_MS,
            next_attempt_at_ms: 0,
            peer_relays: Vec::new(),
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
                    }
                }

                Some(inbound) = self.sync_node.inbound_sync_rx.recv() => {
                    self.on_inbound_sync(inbound).await;
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

                _ = self.shutdown.cancelled() => {
                    info!("Shutting down");
                    break;
                }
            }
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
        match self.allowlist.list_peers().await {
            Ok(peers) => peers.iter().any(|p| &p.node_id == peer_id),
            Err(e) => {
                error!("Failed to read allowlist, denying sync: {}", e);
                false
            }
        }
    }

    /// Handle a file change event from the watcher.
    async fn on_file_changed(&mut self, event: FileEvent) {
        match event.kind {
            FileEventKind::Modified => {
                self.on_file_modified(&event.path).await;
            }
            FileEventKind::Deleted => {
                self.on_file_deleted(&event.path).await;
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
        // when a peer connects — prepare_sync_request trusts .loro state, with no fallback.
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
        if let Err(e) = vault.save_registry().await {
            error!("Failed to persist registry after file change for {}: {}", path, e);
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
        let request_bytes = match vault.prepare_sync_request().await {
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
            SyncExchangeKind::NeighborUp,
        );
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
        let request_bytes = match vault.prepare_sync_request().await {
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
            SyncExchangeKind::ChangePull { path },
        );
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

    /// Handle an inbound sync request from a remote peer (via QUIC bi-stream).
    async fn on_inbound_sync(&mut self, inbound: InboundSyncRequest) {
        if !self.is_peer_allowed(&inbound.remote_id).await {
            warn!(peer = %inbound.remote_id, "Inbound sync from non-allowlisted peer, dropping");
            // Dropping reply_tx closes the QUIC stream without a response.
            drop(inbound.reply_tx);
            return;
        }

        let vault = self.vault.lock().await;
        match vault.process_sync_message(&inbound.message_bytes).await {
            Ok((response_bytes_opt, modified_paths)) => {
                if !modified_paths.is_empty() {
                    info!("Applied {} file(s) from inbound sync", modified_paths.len());
                }

                self.peer_registry
                    .lock()
                    .await
                    .update_last_seen(&inbound.remote_id);
                if let Err(e) = self
                    .allowlist
                    .update_last_seen(&inbound.remote_id, now_ms())
                    .await
                {
                    warn!(
                        "Failed to update allowlist last_seen for {}: {}",
                        inbound.remote_id, e
                    );
                }

                if let Some(resp_bytes) = response_bytes_opt {
                    // reply_tx being dropped closes the stream without a response — that's fine.
                    let _ = inbound.reply_tx.send(resp_bytes);
                }
            }
            Err(e) => {
                error!("Failed to process inbound sync message: {}", e);
            }
        }
    }
}

#[cfg(test)]
mod backoff_tests {
    use super::{RECONNECT_BASE_MS, RECONNECT_CAP_MS, next_reconnect_backoff};

    /// Backoff doubles from the base, saturates at the cap, and never overflows.
    #[test]
    fn reconnect_backoff_grows_and_caps() {
        // Doubling from the base.
        assert_eq!(
            next_reconnect_backoff(RECONNECT_BASE_MS),
            RECONNECT_BASE_MS * 2
        );

        // Climbs toward the cap, then saturates there.
        let mut cur = RECONNECT_BASE_MS;
        for _ in 0..20 {
            cur = next_reconnect_backoff(cur);
            assert!(cur <= RECONNECT_CAP_MS, "backoff must never exceed the cap");
        }
        assert_eq!(cur, RECONNECT_CAP_MS, "backoff should reach the cap");
        assert_eq!(
            next_reconnect_backoff(RECONNECT_CAP_MS),
            RECONNECT_CAP_MS,
            "backoff at the cap stays at the cap"
        );

        // Saturating multiply guards against overflow near u64::MAX.
        assert_eq!(next_reconnect_backoff(u64::MAX), RECONNECT_CAP_MS);
    }
}
