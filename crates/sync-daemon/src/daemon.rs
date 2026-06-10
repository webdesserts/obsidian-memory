//! Daemon event loop extracted as a library entry point.
//!
//! This allows the `memory` binary to embed the sync daemon behind a feature
//! flag, while still keeping the standalone `sync-daemon` binary as a thin wrapper.

use crate::allowlist::FileAllowlistStorage;
use crate::daemon_lock::DaemonLock;
use crate::http;
use crate::native_fs::NativeFs;
use crate::pair_api::{
    ConnectionState, DaemonCommand, DaemonControl, DaemonStatus, PAIRING_BROADCAST_CAPACITY,
    PairingUiEvent, PeerSummary,
};
use crate::persistence::DaemonConfig;
use crate::relay::EmbeddedRelay;
use crate::watcher::{FileEvent, FileEventKind, FileWatcher};
use anyhow::{Context, Result};
use iroh::EndpointId;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::{broadcast, mpsc, oneshot, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};

use sync_core::allowlist::{AllowedPeer, AllowlistStorage};
use sync_core::fs::FileSystem;
use sync_core::network::pairing::pair_with_mesh_interactive;
use sync_core::network::{
    SyncNode,
    discovery::MeshMetadata,
    gossip::{GossipEvent, VaultGossip},
    pairing::{InboundPairingExchange, PairingApproval, PairingEvent},
    streams::InboundSyncRequest,
};
use sync_core::pairing::{PairingChallenge, PairingHello, PairingSession};
use sync_core::{PeerId, PeerRegistry, Vault};

/// How long the initiator window scans for nearby meshes before stopping.
const INITIATOR_DISCOVERY_TIMEOUT_SECS: u64 = 10;

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or(0)
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
    sync_node: SyncNode,
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
}

/// A completed initiator pairing exchange, routed from the spawned pairing task
/// back to the event loop so the post-pair onboarding (allowlist write, VaultId
/// adoption + gossip re-join, relay persist) runs under `&mut self`.
///
/// Only a *completed* `PairingResult` travels this channel — whether it
/// succeeded or failed the HMAC check. Cancellation and connection errors reply
/// directly from the spawned task and never reach here, so the event loop only
/// ever sees outcomes that warrant adoption work.
struct InitiatorPairOutcome {
    result: sync_core::pairing::PairingResult,
    responder_device_name: String,
}

/// State tracked between `StartDiscovery`, `RequestPairing`, and `SubmitCode`
/// for an in-flight initiator pairing session.
///
/// The discovery task writes to `discovered` as mDNS produces results.
/// `RequestPairing` resolves the peer, parks the QUIC connection, and stores
/// `code_tx`. `SubmitCode` stores `submit_reply` then fires `code_tx` to
/// unblock the parked task.
struct InitiatorSession {
    /// Maps `vault_id` to the first observed peer's endpoint, used by
    /// `RequestPairing` to dial the mesh.
    discovered: Arc<Mutex<HashMap<String, EndpointId>>>,
    /// Cancels the discovery scan and any in-progress pairing attempt.
    cancel: CancellationToken,
    /// Filled by `RequestPairing` after the parked task spawns. `SubmitCode`
    /// takes it to unblock the `get_code` callback with the typed code.
    code_tx: Option<oneshot::Sender<String>>,
    /// Stored by `SubmitCode` so the final `PairingResult` routes back to the
    /// Pair button's reply after post-pair onboarding. Taken by
    /// `on_initiator_pair_outcome`.
    submit_reply: Option<oneshot::Sender<Result<String, String>>>,
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
        }
    }

    /// Seed the active initiator's discovered map for testing.
    ///
    /// Integration tests have no mDNS, so there's no natural way for the
    /// discovered map to be populated. This helper pre-populates `vault_id →
    /// endpoint_id` so tests can drive `RequestPairing` without real mDNS.
    ///
    /// This method is `pub` solely for integration test access — use only in
    /// test code. Production callers should go through `StartDiscovery`.
    pub async fn test_seed_discovered(&mut self, vault_id: String, endpoint_id: iroh::EndpointId) {
        if self.active_initiator.is_none() {
            let cancel = CancellationToken::new();
            self.active_initiator = Some(InitiatorSession {
                discovered: Arc::new(Mutex::new(HashMap::new())),
                cancel,
                code_tx: None,
                submit_reply: None,
            });
        }
        if let Some(ref session) = self.active_initiator {
            session.discovered.lock().await.insert(vault_id, endpoint_id);
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

    /// Begin mDNS discovery for the initiator window.
    ///
    /// Cancels any prior initiator session, then spawns a task that subscribes
    /// to mDNS for `INITIATOR_DISCOVERY_TIMEOUT_SECS` and forwards each
    /// `DiscoveredMesh` to `reply`. The same task records `vault_id` →
    /// `EndpointId` in `active_initiator.discovered` so a subsequent
    /// `SubmitCode` can resolve the peer without re-scanning.
    ///
    /// Closing `reply` (by dropping the sender at the end of the task) signals
    /// "discovery finished" to the desktop side, which then surfaces the
    /// `pair://discovery-finished` event to the window.
    async fn start_initiator_discovery(
        &mut self,
        reply: mpsc::UnboundedSender<sync_core::network::discovery::DiscoveredMesh>,
    ) {
        // Replace any prior session, cancelling its in-flight tasks.
        if let Some(prev) = self.active_initiator.take() {
            prev.cancel.cancel();
        }

        let cancel = CancellationToken::new();
        let discovered = Arc::new(Mutex::new(HashMap::<String, EndpointId>::new()));

        self.active_initiator = Some(InitiatorSession {
            discovered: discovered.clone(),
            cancel: cancel.clone(),
            code_tx: None,
            submit_reply: None,
        });

        let Some(stream) = self.sync_node.subscribe_discovery().await else {
            debug!("mDNS discovery not available on this platform; closing reply");
            drop(reply);
            return;
        };

        tokio::spawn(async move {
            use futures::StreamExt;
            use sync_core::network::discovery::{DiscoveredMesh, DiscoveryEvent, MeshMetadata};

            let deadline = tokio::time::sleep(std::time::Duration::from_secs(
                INITIATOR_DISCOVERY_TIMEOUT_SECS,
            ));
            futures::pin_mut!(stream);
            futures::pin_mut!(deadline);

            loop {
                tokio::select! {
                    Some(event) = stream.next() => {
                        if let DiscoveryEvent::Discovered { endpoint_info, .. } = event {
                            let metadata = endpoint_info
                                .data
                                .user_data()
                                .and_then(|ud| {
                                    serde_json::from_str::<MeshMetadata>(ud.as_ref()).ok()
                                });

                            if let Some(meta) = metadata {
                                let endpoint_id = endpoint_info.endpoint_id;

                                // Dedupe by vault_id: only emit on first sighting.
                                // mDNS re-advertises every ~5s, so without this guard
                                // the UI would receive a flood of identical events.
                                // This is stricter than `pair.rs` — the CLI tracks
                                // additional peers in the same mesh and updates
                                // `online_count`. Phase 1.5's UI displays only the
                                // mesh name + a "1 online" hint, so the first sighting
                                // is enough; a richer peer count is Phase 6 work.
                                let mut map = discovered.lock().await;
                                let is_new = !map.contains_key(&meta.vid);
                                map.entry(meta.vid.clone()).or_insert(endpoint_id);
                                drop(map);

                                if !is_new {
                                    continue;
                                }

                                let mesh = DiscoveredMesh {
                                    mesh_name: meta.mesh.clone(),
                                    vault_id: meta.vid.clone(),
                                    peers: vec![endpoint_id],
                                    online_count: 1,
                                };

                                // Send failures mean the desktop dropped the
                                // receiver (window closed) — stop the scan early.
                                if reply.send(mesh).is_err() {
                                    debug!("Initiator discovery reply channel closed; ending scan");
                                    return;
                                }
                            }
                        }
                    }
                    _ = &mut deadline => {
                        debug!("Initiator discovery scan window elapsed");
                        return;
                    }
                    _ = cancel.cancelled() => {
                        debug!("Initiator discovery cancelled");
                        return;
                    }
                }
            }
        });
    }

    /// Connect to the selected mesh's peer and park the QUIC connection open.
    ///
    /// Step 1 of the two-step GUI pairing flow. Resolves the peer endpoint from
    /// the active discovery session, spawns a task that opens the QUIC connection
    /// and sends `PairingHello` (triggering the responder to generate + display its
    /// code), then parks awaiting a code delivered later by `SubmitCode`. On
    /// connect, `reply` receives `Ok(responder_device_name)` — the UI's cue to
    /// reveal the code entry step. Connect errors reply `Err(...)` directly.
    async fn request_initiator_pairing(
        &mut self,
        vault_id: String,
        reply: oneshot::Sender<Result<String, String>>,
    ) {
        let Some(session) = self.active_initiator.as_mut() else {
            let _ = reply.send(Err(
                "No active pairing session. Click 'Pair with nearby device…' first.".to_string(),
            ));
            return;
        };

        let peer_endpoint_id = {
            let map = session.discovered.lock().await;
            map.get(&vault_id).copied()
        };

        let Some(peer_endpoint_id) = peer_endpoint_id else {
            let _ = reply.send(Err(
                "Selected mesh has no discovered peers yet. Wait for discovery to find a peer."
                    .to_string(),
            ));
            return;
        };

        // Cancel any prior parked pairing attempt before spawning a fresh one.
        // (Covers re-request after a failed connect.)
        session.cancel.cancel();
        let cancel = CancellationToken::new();
        session.cancel = cancel.clone();
        session.code_tx = None;
        session.submit_reply = None;

        let (code_tx, code_rx) = oneshot::channel::<String>();
        session.code_tx = Some(code_tx);

        let endpoint = self.sync_node.endpoint.clone();
        let self_node_id = *self.sync_node.node_id().as_bytes();
        let device_name = self.device_name.clone();
        let outcome_tx = self.initiator_outcome_tx.clone();

        tokio::spawn(async move {
            let exchange = tokio::select! {
                r = run_initiator_pairing_parked(
                    &endpoint,
                    peer_endpoint_id,
                    self_node_id,
                    &device_name,
                    reply,
                    code_rx,
                ) => r,
                _ = cancel.cancelled() => {
                    // Cancellation drops code_rx, which wakes any awaiting code_tx.send()
                    // in SubmitCode with a SendError (harmless — SubmitCode checks for that).
                    return;
                }
            };

            match exchange {
                Some((result, responder_device_name)) => {
                    // A completed exchange (success or bad HMAC) must be onboarded under
                    // `&mut self`. If the send fails the daemon is shutting down.
                    let outcome = InitiatorPairOutcome {
                        result,
                        responder_device_name,
                    };
                    let _ = outcome_tx.send(outcome);
                }
                None => {
                    // Connect error — already replied via connect_reply inside the function.
                }
            }
        });
    }

    /// Deliver the typed 6-digit code into the parked pairing request.
    ///
    /// Step 2 of the two-step GUI pairing flow. Takes `code_tx` from the session
    /// (set by `RequestPairing`) to unblock the parked `get_code` callback.
    /// Stores `reply` on the session so `on_initiator_pair_outcome` can route the
    /// final `PairingResult` back to the Pair button after post-pair onboarding.
    async fn submit_initiator_code(
        &mut self,
        vault_id: String,
        code: String,
        reply: oneshot::Sender<Result<String, String>>,
    ) {
        let Some(session) = self.active_initiator.as_mut() else {
            let _ = reply.send(Err(
                "No active pairing session. Click 'Pair with nearby device…' first.".to_string(),
            ));
            return;
        };

        // Verify the code is for the mesh that was selected at request time.
        // This fires when the vault_id doesn't match the discovered map — e.g.
        // a stale window submitting a code for a mesh that was never discovered
        // in this session.
        {
            let map = session.discovered.lock().await;
            if !map.contains_key(&vault_id) {
                let _ = reply.send(Err(
                    "That mesh is no longer the active pairing target.".to_string(),
                ));
                return;
            }
        }

        let Some(code_tx) = session.code_tx.take() else {
            let _ = reply.send(Err(
                "No pairing request in progress. Click 'Request pairing' first.".to_string(),
            ));
            return;
        };

        // Store the Pair button's reply before unblocking the parked task. The
        // outcome channel routes the PairingResult back here after onboarding.
        session.submit_reply = Some(reply);

        if code_tx.send(code).is_err() {
            // The parked task has already exited (connect error or cancellation).
            let reply = session.submit_reply.take();
            if let Some(r) = reply {
                let _ = r.send(Err(
                    "Pairing request is no longer active. Try again.".to_string(),
                ));
            }
        }
    }

    /// Run the shared post-pair onboarding for a completed initiator exchange,
    /// then reply to the desktop Pair button's oneshot.
    ///
    /// Runs on the event loop (`&mut self`) so it can adopt the mesh VaultId and
    /// re-join gossip in place. The shared helper writes the allowlist; on a
    /// successful pair we then adopt + re-join + persist the relay. A failed HMAC
    /// check replies with the standard "wrong/expired code" error. Every path
    /// sends exactly one reply.
    async fn on_initiator_pair_outcome(&mut self, outcome: InitiatorPairOutcome) {
        let InitiatorPairOutcome {
            result,
            responder_device_name,
        } = outcome;

        // Recover the Pair button's reply oneshot from the session. If absent
        // (race: session was cancelled between SubmitCode and here), log + drop.
        let reply = self
            .active_initiator
            .as_mut()
            .and_then(|s| s.submit_reply.take());

        let Some(reply) = reply else {
            warn!("on_initiator_pair_outcome: no submit_reply to route result (session cancelled?)");
            return;
        };

        if !result.success {
            let _ = reply.send(Err(
                "Pairing failed. The code may be wrong or expired. Try again.".to_string(),
            ));
            return;
        }

        let self_peer_id = PeerId::from_bytes(*self.sync_node.node_id().as_bytes());
        crate::pair_shared::write_pair_allowlist(
            self.allowlist.as_ref(),
            self_peer_id,
            &self.device_name,
            &result.mesh_members,
        )
        .await;

        // Recover the mesh VaultId from the topic and adopt it: rewrite
        // metadata.toml, re-join the mesh's gossip topic, re-publish mDNS.
        //
        // A successful pair without a vault topic is protocol-impossible — the
        // responder always sends one — but structurally possible. Treat it as a
        // failure rather than a silent success: skipping adoption would land the
        // device on the wrong gossip topic, so pairing "succeeds" but sync never
        // works. Surfacing the error lets the user retry instead.
        let Some(new_vault_id) = crate::pair_shared::vault_id_from_pairing_topic(result.vault_topic)
        else {
            error!("Pairing succeeded but the mesh did not provide a vault topic");
            let _ = reply.send(Err(
                "Paired, but the mesh did not provide a vault topic. Try again.".to_string(),
            ));
            return;
        };

        if let Err(e) = self
            .adopt_and_rejoin(new_vault_id, result.mesh_members.clone())
            .await
        {
            error!("Failed to adopt mesh VaultId after pairing: {:#}", e);
            let _ = reply.send(Err(format!("Paired, but failed to join the mesh: {e:#}")));
            return;
        }

        // Persist the mesh's relay URL for the next daemon start (not a live
        // endpoint rebuild — see `persist_adopted_relay`).
        crate::pair_shared::persist_adopted_relay(&self.vault_path, &result.relay_urls).await;

        let _ = reply.send(Ok(responder_device_name));
    }

    /// Adopt a new VaultId and re-subscribe to its gossip topic at runtime.
    ///
    /// Used after a pairing initiator joins an existing mesh: the device must
    /// abandon its own VaultId and land on the mesh's gossip topic so a
    /// `NeighborUp` fires and the full-sync pull begins. Steps:
    /// 1. Rewrite `.sync/metadata.toml` + in-memory id (`Vault::adopt_vault_id`).
    /// 2. Join gossip on the new topic, bootstrapping off the mesh members.
    /// 3. Swap `self.vault_gossip` — dropping the old `VaultGossip` auto-leaves
    ///    the old topic (iroh-gossip leaves once both sender + receiver drop).
    /// 4. Re-publish mDNS so LAN discovery groups us under the new VaultId.
    ///
    /// Runs to completion within a single event-loop turn, so the next
    /// `run_loop` `select!` re-borrows the freshly-swapped `self.vault_gossip`.
    pub async fn adopt_and_rejoin(
        &mut self,
        new_vault_id: sync_core::VaultId,
        mesh_members: Vec<PeerId>,
    ) -> Result<()> {
        // 1. Adopt the VaultId (metadata.toml + in-memory).
        self.vault.lock().await.adopt_vault_id(new_vault_id).await?;

        // 2. Bootstrap gossip off the mesh members (same filter-map as startup).
        let bootstrap_ids: Vec<EndpointId> = mesh_members
            .iter()
            .filter_map(|p| {
                EndpointId::from_bytes(p.as_bytes())
                    .map_err(|e| warn!("Skipping invalid mesh member for gossip bootstrap: {}", e))
                    .ok()
            })
            .collect();

        // 3. Join the new topic and swap — the old VaultGossip drops here,
        //    auto-leaving the old topic.
        let new_gossip = self
            .sync_node
            .join_vault_gossip(&new_vault_id, bootstrap_ids)
            .await
            .context("Failed to re-join vault gossip on adopted VaultId")?;
        self.vault_gossip = new_gossip;
        info!(vault_id = %new_vault_id, "Re-joined gossip on adopted VaultId");

        // 4. Re-publish mDNS under the new VaultId so LAN peers regroup us.
        let mesh = self
            .mesh_name
            .clone()
            .unwrap_or_else(|| self.device_name.clone());
        let mesh_metadata = MeshMetadata {
            mesh,
            vid: new_vault_id.to_string(),
            ver: 1,
        };
        let relay_url = self
            .relay_url
            .as_ref()
            .and_then(|u| u.parse::<iroh::RelayUrl>().ok());
        self.sync_node
            .publish_mesh_info(&mesh_metadata, relay_url.as_ref());

        self.emit_status().await;
        Ok(())
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
    async fn on_file_deleted(&mut self, path: &str) {
        info!("File deleted: {}", path);

        let vault = self.vault.lock().await;

        if vault.consume_sync_flag(path) {
            debug!("Skipping broadcast for synced deletion: {}", path);
            return;
        }

        if let Err(e) = vault.delete_file(path).await {
            error!("Failed to delete file {}: {}", path, e);
            return;
        }

        drop(vault);

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
    async fn on_file_modified(&mut self, path: &str) {
        let vault = self.vault.lock().await;

        if vault.consume_sync_flag(path) {
            debug!("Skipping broadcast for synced file: {}", path);
            return;
        }

        // Always update vault state so .loro files stay current for future sync exchanges.
        // Without this, edits made while no peers are connected would be silently lost
        // when a peer connects — prepare_sync_request trusts .loro state, with no fallback.
        if let Err(e) = vault.on_file_changed(path).await {
            error!("Failed to process file change for {}: {}", path, e);
            return;
        }

        drop(vault);

        if self.peer_registry.lock().await.alive_count() == 0 {
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
        let vault = self.vault.clone();
        let allowlist = self.allowlist.clone();
        let peer_registry = self.peer_registry.clone();
        let endpoint = self.sync_node.endpoint.clone();

        tokio::spawn(async move {
            let response_bytes = match sync_core::network::streams::connect_and_sync_raw(
                &endpoint,
                node_id,
                &request_bytes,
            )
            .await
            {
                Ok(bytes) => bytes,
                Err(e) => {
                    // NeighborUp fires before the peer's QUIC listener is ready on some
                    // OS configurations. Log a warning and move on — the peer will initiate
                    // a sync in the other direction.
                    warn!("Failed to connect for sync with {}: {}", node_id, e);
                    return;
                }
            };

            let vault = vault.lock().await;
            match vault.process_sync_message(&response_bytes).await {
                Ok((_, modified_paths)) => {
                    peer_registry.lock().await.update_last_seen(&peer_id);
                    if let Err(e) = allowlist.update_last_seen(&peer_id, now_ms()).await {
                        warn!(
                            "Failed to update allowlist last_seen for {}: {}",
                            node_id, e
                        );
                    }
                    if !modified_paths.is_empty() {
                        info!(
                            "Synced {} file(s) from {} on NeighborUp",
                            modified_paths.len(),
                            node_id
                        );
                    } else {
                        debug!("Full sync with {} complete — no changes", node_id);
                    }
                }
                Err(e) => {
                    error!("Failed to process sync response from {}: {}", node_id, e);
                }
            }
        });
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

        let vault = self.vault.clone();
        let allowlist = self.allowlist.clone();
        let peer_registry = self.peer_registry.clone();
        let endpoint = self.sync_node.endpoint.clone();

        tokio::spawn(async move {
            let response_bytes = match sync_core::network::streams::connect_and_sync_raw(
                &endpoint,
                from,
                &request_bytes,
            )
            .await
            {
                Ok(bytes) => bytes,
                Err(e) => {
                    warn!("Failed to pull change from {}: {}", from, e);
                    return;
                }
            };

            let vault = vault.lock().await;
            match vault.process_sync_message(&response_bytes).await {
                Ok((_, modified_paths)) => {
                    peer_registry.lock().await.update_last_seen(&peer_id);
                    if let Err(e) = allowlist.update_last_seen(&peer_id, now_ms()).await {
                        warn!("Failed to update allowlist last_seen for {}: {}", from, e);
                    }
                    if !modified_paths.is_empty() {
                        info!(
                            "Pulled {} file(s) from {} after change notification on {}",
                            modified_paths.len(),
                            from,
                            path
                        );
                    }
                }
                Err(e) => {
                    error!("Failed to process pulled change from {}: {}", from, e);
                }
            }
        });
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

/// Drive the initiator pairing protocol against a discovered peer, parking
/// between connect and code submission.
///
/// This is the pure pairing-exchange driver for the two-step GUI flow. It has
/// **no** allowlist or adoption side effects. The function:
/// 1. Opens the QUIC connection and sends `PairingHello` (triggering the
///    responder to generate + display its 6-digit code).
/// 2. Fires `connect_reply` `Ok(responder_device_name)` to signal the GUI to
///    reveal the code entry step.
/// 3. Parks awaiting a code delivered via `code_rx` (filled by `SubmitCode`).
/// 4. Sends `PairingResponse { hmac(code) }` and awaits `PairingResult`.
///
/// Returns `Some((result, responder_device_name))` on a completed exchange
/// (success or failed HMAC check). Returns `None` when a connection error
/// occurs — `connect_reply` carries the `Err` in that case.
async fn run_initiator_pairing_parked(
    endpoint: &iroh::Endpoint,
    peer_endpoint_id: EndpointId,
    self_node_id_bytes: [u8; 32],
    device_name: &str,
    connect_reply: oneshot::Sender<Result<String, String>>,
    code_rx: oneshot::Receiver<String>,
) -> Option<(sync_core::pairing::PairingResult, String)> {
    let self_peer_id = PeerId::from_bytes(self_node_id_bytes);
    let hello = PairingHello {
        node_id: self_peer_id,
        device_name: device_name.to_string(),
    };

    // The PairingChallenge carries the responder's device_name; capture it
    // here so we can return it to the UI's success message.
    let captured_device_name = Arc::new(Mutex::new(String::new()));
    let captured_device_name_setter = captured_device_name.clone();

    // `connect_reply` must be fired exactly once. Move it into an Option so
    // the error path can take it without risk of a second fire.
    let connect_reply_cell = Arc::new(Mutex::new(Some(connect_reply)));
    let connect_reply_for_closure = connect_reply_cell.clone();

    let result = pair_with_mesh_interactive(
        endpoint,
        peer_endpoint_id,
        &hello,
        move |challenge| {
            let setter = captured_device_name_setter.clone();
            let reply_cell = connect_reply_for_closure.clone();
            let code_rx = code_rx; // move into closure — runs exactly once
            async move {
                let responder_name = challenge.device_name.clone();
                *setter.lock().await = responder_name.clone();

                // Signal the GUI: connection established, responder is showing its code.
                if let Some(reply) = reply_cell.lock().await.take() {
                    let _ = reply.send(Ok(responder_name));
                }

                // Park here until SubmitCode delivers the typed code.
                code_rx
                    .await
                    .map_err(|_| anyhow::anyhow!("pairing cancelled before code was entered"))
            }
        },
    )
    .await;

    match result {
        Ok(pairing_result) => {
            let responder_device_name = captured_device_name.lock().await.clone();
            Some((pairing_result, responder_device_name))
        }
        Err(e) => {
            // Connection error — fire connect_reply with the error if not already sent.
            if let Some(reply) = connect_reply_cell.lock().await.take() {
                let _ = reply.send(Err(format!("Pairing connection failed: {e:#}")));
            }
            None
        }
    }
}

/// Start the daemon and return a control handle before the event loop begins.
///
/// Unlike `run_with_shutdown`, this function splits startup from the event loop:
///
/// - The **outer `Result`** covers everything up to and including `Daemon::new()` —
///   lock acquire, vault load, identity load, relay start, SyncNode creation, mDNS
///   publish, gossip join, health endpoint, file watcher. Startup failures bubble up
///   as `Err` before any `DaemonControl` is materialized.
/// - **`DaemonControl`** is yielded after `Daemon::new()` returns successfully, giving
///   the caller a way to observe status and drive pairing.
/// - The **inner `JoinHandle<Result<()>>`** owns the event loop and graceful shutdown.
///   Its `Result` covers only runtime errors (not startup failures).
///
/// If `shutdown` fires during startup, the function returns `Ok(...)` with the join
/// handle already resolved — callers can safely await it.
pub async fn run_with_shutdown_controlled(
    config: DaemonRunConfig,
    shutdown: CancellationToken,
) -> Result<(DaemonControl, JoinHandle<Result<()>>)> {
    // Run startup. If `shutdown` fires before startup completes, return a no-op handle.
    let startup_result = tokio::select! {
        result = startup_inner(&config, shutdown.clone()) => result,
        _ = shutdown.cancelled() => {
            // Cancel during startup — lock cleanup via RAII, relay_url cleanup deferred
            // (same reasoning as in run_with_shutdown).
            info!("Daemon shutdown requested during startup — exiting cleanly");
            // Return a no-op handle that resolves immediately.
            let handle = tokio::spawn(async { Ok(()) });
            let (status_tx, status_rx) = watch::channel(DaemonStatus::initial());
            let (pairing_tx, pairing_rx) = broadcast::channel(PAIRING_BROADCAST_CAPACITY);
            let (command_tx, _command_rx) = mpsc::unbounded_channel();
            drop(status_tx);
            drop(pairing_tx);
            let control = DaemonControl { status_rx, pairing_rx, command_tx };
            return Ok((control, handle));
        }
    };

    let StartupBundle {
        mut daemon,
        embedded_relay,
        mut daemon_config,
        vault_path,
        mesh_name,
        _daemon_lock,
        _watcher,
    } = startup_result?;

    // Wire control channels into the daemon before the loop starts.
    let (status_tx, status_rx) = watch::channel(DaemonStatus::initial());
    let (pairing_tx, pairing_rx) = broadcast::channel(PAIRING_BROADCAST_CAPACITY);
    let (command_tx, command_rx) = mpsc::unbounded_channel();

    daemon.wire_control(status_tx, pairing_tx.clone(), command_rx, mesh_name);

    // Emit the initial status (Idle, 0 peers, relay URL set) so the tray has something
    // to display immediately without waiting for the first peer event.
    daemon.emit_status().await;

    let control = DaemonControl {
        status_rx,
        pairing_rx,
        command_tx,
    };

    let handle = tokio::spawn(async move {
        // DaemonLock and FileWatcher are moved here so they live for the duration
        // of run_loop. The underscore prefix silences the "unused" warning while
        // making the RAII intent explicit — both must not drop before run_loop exits.
        let _daemon_lock = _daemon_lock;
        let _watcher = _watcher;

        daemon.run_loop().await;
        // Graceful shutdown
        if let Err(e) = daemon.sync_node.shutdown().await {
            warn!("Error during iroh node shutdown: {}", e);
        }
        if let Some(relay) = embedded_relay {
            relay.shutdown().await;
            if let Err(e) = daemon_config.set_relay_url(None, &vault_path) {
                warn!("Failed to clear relay URL from daemon.toml: {}", e);
            }
        }
        Ok(())
    });

    Ok((control, handle))
}

/// Run the sync daemon with the given configuration, honoring an externally-supplied
/// cancellation token.
///
/// This is the embedding entry point used by the desktop app, which needs to drive
/// shutdown from a tray-quit click rather than an OS signal. Cancelling `shutdown`
/// at any point — including during startup — causes the function to return cleanly.
///
/// The entire startup sequence is wrapped in a single `tokio::select!` so that a
/// quit-during-startup (e.g. slow gossip join) doesn't hang the caller.
pub async fn run_with_shutdown(config: DaemonRunConfig, shutdown: CancellationToken) -> Result<()> {
    tokio::select! {
        result = run_inner(config, shutdown.clone()) => result,
        _ = shutdown.cancelled() => {
            // Shutdown fired before the inner future resolved — startup was interrupted.
            // Lock cleanup is handled via RAII (DaemonLock drops when run_inner is
            // cancelled). relay_url cleanup (clearing daemon.toml) is deferred: Phase 1
            // always sets relay_listen: None so relay_url is never written here, making
            // cleanup a no-op. TODO(phase-2): when relay_listen is wired, load
            // daemon_config before the select! and call set_relay_url(None) here so a
            // cancel-during-startup doesn't leave a stale relay URL in daemon.toml.
            info!("Daemon shutdown requested during startup — exiting cleanly");
            Ok(())
        }
    }
}

/// Run the sync daemon with the given configuration.
///
/// This is the main entry point for embedding the daemon in the `memory` binary.
/// Assumes logging is already configured by the caller.
pub async fn run(config: DaemonRunConfig) -> Result<()> {
    let token = CancellationToken::new();
    let signal_token = token.clone();
    tokio::spawn(async move {
        memory_common::shutdown_signal().await;
        signal_token.cancel();
    });
    run_with_shutdown(config, token).await
}

/// Everything `startup_inner` returns to the caller after a successful startup.
///
/// Keeps `DaemonLock` and `FileWatcher` alive until they are explicitly moved into
/// the `run_loop` task — if either were dropped at the call site, the OS-level lock
/// and file watcher would stop before the daemon processes any events.
struct StartupBundle {
    daemon: Daemon<NativeFs, FileAllowlistStorage>,
    embedded_relay: Option<EmbeddedRelay>,
    daemon_config: DaemonConfig,
    vault_path: PathBuf,
    mesh_name: String,
    /// Holds the exclusive flock on `.sync/daemon.lock` for the daemon's lifetime.
    _daemon_lock: DaemonLock,
    /// Keeps the OS-level file watcher alive; dropping it stops event delivery.
    _watcher: FileWatcher,
}

/// Startup phase: lock acquisition, vault init, node startup.
///
/// Returns a [`StartupBundle`] that includes `DaemonLock` and `FileWatcher` so
/// the caller can move them into the spawned `run_loop` task, keeping both alive
/// for the daemon's full lifetime. All startup failures surface as `Err` before
/// any `StartupBundle` is materialized.
///
/// Used by `run_with_shutdown_controlled`; the event loop is run by the caller.
async fn startup_inner(
    config: &DaemonRunConfig,
    shutdown: CancellationToken,
) -> Result<StartupBundle> {
    // Acquire exclusive daemon lock — must outlive this function. It is moved into
    // the StartupBundle and from there into the spawned run_loop task.
    let daemon_lock = DaemonLock::acquire(&config.vault).context(
        "Failed to acquire daemon lock — is another daemon already running on this vault?",
    )?;

    info!("Starting sync daemon");
    info!("Vault path: {:?}", config.vault);

    let fs = NativeFs::new(config.vault.clone());

    // Load identity before the vault so we can author Loro ops under this
    // device's PeerId (config-load has no dependency on the vault).
    let (mut daemon_config, identity_key) =
        DaemonConfig::load_or_generate(&config.vault, config.identity_key.as_deref()).await?;

    info!("Daemon PeerId: {}", daemon_config.peer_id);

    let author = identity_key.peer_id();
    let vault = if fs.exists(".sync").await? {
        info!("Loading existing vault");
        Vault::load(fs, author).await?
    } else {
        info!("Initializing new vault");
        Vault::init(fs, author).await?
    };

    let vault_id = vault.vault_id();
    info!("Vault loaded, vault ID: {}", vault_id);

    if daemon_config.relay_url.is_some() {
        info!("Clearing stale relay URL from previous run");
        if let Err(e) = daemon_config.set_relay_url(None, &config.vault) {
            warn!("Failed to clear stale relay URL: {}", e);
        }
    }

    // Start the embedded relay before the SyncNode so we can pass its URL in.
    // When advertised_relay_url is set, bind on relay_listen but tell peers to dial
    // the advertised address (e.g. LAN IP instead of 0.0.0.0).
    // Failure is non-fatal: the daemon continues without relay support.
    let embedded_relay: Option<EmbeddedRelay> = if let Some(ref addr_str) = config.relay_listen {
        match addr_str.parse() {
            Ok(bind_addr) => {
                let relay_result = if let Some(ref adv_url) = config.advertised_relay_url {
                    EmbeddedRelay::start_with_advertised_url(bind_addr, adv_url).await
                } else {
                    EmbeddedRelay::start(bind_addr).await
                };
                match relay_result {
                    Ok(relay) => {
                        info!(url = %relay.relay_url(), "Embedded relay started");
                        Some(relay)
                    }
                    Err(e) => {
                        warn!("Failed to start embedded relay, continuing without: {}", e);
                        None
                    }
                }
            }
            Err(e) => {
                warn!(
                    "Invalid relay-listen address '{}': {}, continuing without relay",
                    addr_str, e
                );
                None
            }
        }
    } else {
        None
    };

    let relay_url = embedded_relay.as_ref().map(|r| r.relay_url().clone());

    if let Some(ref url) = relay_url {
        if let Err(e) = daemon_config.set_relay_url(Some(url.to_string()), &config.vault) {
            warn!("Failed to persist relay URL to daemon.toml: {}", e);
        }
    }

    let secret_key_bytes = identity_key.secret_key_bytes();
    let allowlist = Arc::new(FileAllowlistStorage::new(&config.vault));

    let sync_node = SyncNode::new(secret_key_bytes, relay_url.as_ref(), allowlist.clone())
        .await
        .context("Failed to create iroh SyncNode")?;

    info!(node_id = %sync_node.node_id(), "Iroh node started");

    let mesh_name = daemon_config.mesh_name.clone().unwrap_or_else(|| {
        config
            .vault
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("Obsidian Vault")
            .to_string()
    });
    let mesh_metadata = MeshMetadata {
        mesh: mesh_name.clone(),
        vid: vault_id.to_string(),
        ver: 1,
    };
    sync_node.publish_mesh_info(&mesh_metadata, relay_url.as_ref());

    let bootstrap_ids: Vec<EndpointId> = match allowlist.list_peers().await {
        Ok(peers) => peers
            .iter()
            .filter_map(|p| {
                EndpointId::from_bytes(p.node_id.as_bytes())
                    .map_err(|e| {
                        warn!(
                            "Skipping invalid allowlist peer for gossip bootstrap: {}",
                            e
                        )
                    })
                    .ok()
            })
            .collect(),
        Err(e) => {
            warn!("Failed to read allowlist for gossip bootstrap: {}", e);
            vec![]
        }
    };

    let vault_gossip = sync_node
        .join_vault_gossip(&vault_id, bootstrap_ids)
        .await
        .context("Failed to join vault gossip topic")?;

    info!("Joined vault gossip topic");

    if let Some(ref health_addr) = config.health_listen {
        let health_addr = health_addr.clone();
        tokio::spawn(async move {
            http::serve_health(&health_addr).await;
        });
        info!(
            "Health endpoint started on {}",
            config.health_listen.as_ref().unwrap()
        );
    }

    let watcher = FileWatcher::new(config.vault.clone())?;
    info!("File watcher started");

    let discovery_rx: Option<
        tokio::sync::mpsc::Receiver<sync_core::network::discovery::DiscoveredMesh>,
    > = {
        use futures::StreamExt;
        use sync_core::network::discovery::{
            DiscoveredMesh, DiscoveryEvent, MeshMetadata as DiscoveryMeshMetadata,
        };

        if let Some(stream) = sync_node.subscribe_discovery().await {
            let (tx, rx) = tokio::sync::mpsc::channel(16);
            tokio::spawn(async move {
                futures::pin_mut!(stream);
                while let Some(event) = stream.next().await {
                    match event {
                        DiscoveryEvent::Discovered { endpoint_info, .. } => {
                            let raw_user_data = endpoint_info
                                .data
                                .user_data()
                                .map(|ud: &iroh::address_lookup::UserData| ud.as_ref().to_string());
                            info!(
                                peer = %endpoint_info.endpoint_id,
                                has_user_data = raw_user_data.is_some(),
                                user_data = ?raw_user_data,
                                "DIAG: mDNS Discovered event"
                            );
                            let metadata = match raw_user_data.as_deref() {
                                Some(s) => match serde_json::from_str::<DiscoveryMeshMetadata>(s) {
                                    Ok(m) => Some(m),
                                    Err(e) => {
                                        info!(
                                            peer = %endpoint_info.endpoint_id,
                                            err = %e,
                                            user_data = %s,
                                            "DIAG: MeshMetadata JSON parse failed"
                                        );
                                        None
                                    }
                                },
                                None => None,
                            };

                            if let Some(meta) = metadata {
                                let mesh = DiscoveredMesh {
                                    mesh_name: meta.mesh.clone(),
                                    vault_id: meta.vid.clone(),
                                    peers: vec![endpoint_info.endpoint_id],
                                    online_count: 1,
                                };
                                info!(
                                    mesh = %mesh.mesh_name,
                                    vid = %mesh.vault_id,
                                    "DIAG: queueing DiscoveredMesh to run_loop"
                                );
                                let _ = tx.try_send(mesh);
                            }
                        }
                        DiscoveryEvent::Expired { endpoint_id } => {
                            debug!(peer = %endpoint_id, "mDNS: peer expired");
                        }
                    }
                }
            });
            Some(rx)
        } else {
            None
        }
    };

    let device_name = {
        let hostname = gethostname::gethostname();
        hostname.to_str().unwrap_or("Sync Daemon").to_string()
    };
    info!(device_name = %device_name, "Device name resolved for pairing");

    // The watcher must outlive this function — it is moved into StartupBundle and
    // from there into the spawned run_loop task. Dropping it would stop OS events.
    let (file_event_rx, watcher) = watcher.into_event_rx();

    let daemon = Daemon::new(
        Arc::new(Mutex::new(vault)),
        sync_node,
        vault_gossip,
        file_event_rx,
        discovery_rx,
        allowlist,
        device_name,
        relay_url.as_ref().map(|u| u.to_string()),
        config.vault.clone(),
        shutdown,
    );

    Ok(StartupBundle {
        daemon,
        embedded_relay,
        daemon_config,
        vault_path: config.vault.clone(),
        mesh_name,
        _daemon_lock: daemon_lock,
        _watcher: watcher,
    })
}

/// Inner daemon body: lock acquisition, vault init, node startup, event loop,
/// and graceful shutdown. Called by `run_with_shutdown`.
async fn run_inner(config: DaemonRunConfig, shutdown: CancellationToken) -> Result<()> {
    // Acquire exclusive daemon lock
    let _daemon_lock = DaemonLock::acquire(&config.vault).context(
        "Failed to acquire daemon lock — is another daemon already running on this vault?",
    )?;

    info!("Starting sync daemon");
    info!("Vault path: {:?}", config.vault);

    let fs = NativeFs::new(config.vault.clone());

    // Load identity before the vault so we can author Loro ops under this
    // device's PeerId (config-load has no dependency on the vault).
    let (mut daemon_config, identity_key) =
        DaemonConfig::load_or_generate(&config.vault, config.identity_key.as_deref()).await?;

    info!("Daemon PeerId: {}", daemon_config.peer_id);

    let author = identity_key.peer_id();
    let vault = if fs.exists(".sync").await? {
        info!("Loading existing vault");
        Vault::load(fs, author).await?
    } else {
        info!("Initializing new vault");
        Vault::init(fs, author).await?
    };

    let vault_id = vault.vault_id();
    info!("Vault loaded, vault ID: {}", vault_id);

    // Clear any stale relay_url left by a previous crash — relay_url is runtime state
    // that must be re-established each run. A stale URL would advertise a dead relay.
    if daemon_config.relay_url.is_some() {
        info!("Clearing stale relay URL from previous run");
        if let Err(e) = daemon_config.set_relay_url(None, &config.vault) {
            warn!("Failed to clear stale relay URL: {}", e);
        }
    }

    // Start the embedded relay before the SyncNode so we can pass its URL in.
    // Failure is non-fatal: the daemon continues without relay support.
    let embedded_relay: Option<EmbeddedRelay> = if let Some(ref addr_str) = config.relay_listen {
        match addr_str.parse() {
            Ok(bind_addr) => match EmbeddedRelay::start(bind_addr).await {
                Ok(relay) => {
                    info!(url = %relay.relay_url(), "Embedded relay started");
                    Some(relay)
                }
                Err(e) => {
                    warn!("Failed to start embedded relay, continuing without: {}", e);
                    None
                }
            },
            Err(e) => {
                warn!(
                    "Invalid relay-listen address '{}': {}, continuing without relay",
                    addr_str, e
                );
                None
            }
        }
    } else {
        None
    };

    let relay_url = embedded_relay.as_ref().map(|r| r.relay_url().clone());

    // Persist the relay URL so plugin peers can discover it.
    // We also clear it on shutdown so the file doesn't advertise a stale URL.
    if let Some(ref url) = relay_url {
        if let Err(e) = daemon_config.set_relay_url(Some(url.to_string()), &config.vault) {
            warn!("Failed to persist relay URL to daemon.toml: {}", e);
        }
    }

    let secret_key_bytes = identity_key.secret_key_bytes();

    // Create the allowlist early so it can be shared with SyncNode (for gossip enforcement)
    // and with the Daemon event loop (for sync and pairing checks).
    let allowlist = Arc::new(FileAllowlistStorage::new(&config.vault));

    let sync_node = SyncNode::new(secret_key_bytes, relay_url.as_ref(), allowlist.clone())
        .await
        .context("Failed to create iroh SyncNode")?;

    info!(node_id = %sync_node.node_id(), "Iroh node started");

    // Publish mesh info via mDNS so LAN peers can discover this mesh.
    // mesh_name defaults to the vault directory name if not set in config.
    let mesh_name = daemon_config.mesh_name.clone().unwrap_or_else(|| {
        config
            .vault
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("Obsidian Vault")
            .to_string()
    });
    let mesh_metadata = MeshMetadata {
        mesh: mesh_name,
        vid: vault_id.to_string(),
        ver: 1,
    };
    sync_node.publish_mesh_info(&mesh_metadata, relay_url.as_ref());

    // Bootstrap gossip from allowlist peers so we reconnect to known devices on startup
    // without requiring manually configured peer addresses.
    let bootstrap_ids: Vec<EndpointId> = match allowlist.list_peers().await {
        Ok(peers) => peers
            .iter()
            .filter_map(|p| {
                // PeerId and EndpointId are both 32-byte ed25519 public keys.
                EndpointId::from_bytes(p.node_id.as_bytes())
                    .map_err(|e| {
                        warn!(
                            "Skipping invalid allowlist peer for gossip bootstrap: {}",
                            e
                        )
                    })
                    .ok()
            })
            .collect(),
        Err(e) => {
            warn!("Failed to read allowlist for gossip bootstrap: {}", e);
            vec![]
        }
    };

    let vault_gossip = sync_node
        .join_vault_gossip(&vault_id, bootstrap_ids)
        .await
        .context("Failed to join vault gossip topic")?;

    info!("Joined vault gossip topic");

    // Optionally start the health endpoint
    if let Some(ref health_addr) = config.health_listen {
        let health_addr = health_addr.clone();
        tokio::spawn(async move {
            http::serve_health(&health_addr).await;
        });
        info!(
            "Health endpoint started on {}",
            config.health_listen.as_ref().unwrap()
        );
    }

    let watcher = FileWatcher::new(config.vault.clone())?;
    info!("File watcher started");

    // Spawn mDNS discovery events into a channel so the main select loop stays uniform.
    // Discovery is native-only; the channel is None when mDNS is unavailable.
    let discovery_rx: Option<
        tokio::sync::mpsc::Receiver<sync_core::network::discovery::DiscoveredMesh>,
    > = {
        use futures::StreamExt;
        use sync_core::network::discovery::{
            DiscoveredMesh, DiscoveryEvent, MeshMetadata as DiscoveryMeshMetadata,
        };

        if let Some(stream) = sync_node.subscribe_discovery().await {
            let (tx, rx) = tokio::sync::mpsc::channel(16);
            tokio::spawn(async move {
                futures::pin_mut!(stream);
                while let Some(event) = stream.next().await {
                    match event {
                        DiscoveryEvent::Discovered { endpoint_info, .. } => {
                            let metadata = endpoint_info.data.user_data().and_then(|ud: &iroh::address_lookup::UserData| {
                                serde_json::from_str::<DiscoveryMeshMetadata>(ud.as_ref()).ok()
                            });

                            if let Some(meta) = metadata {
                                let mesh = DiscoveredMesh {
                                    mesh_name: meta.mesh.clone(),
                                    vault_id: meta.vid.clone(),
                                    peers: vec![endpoint_info.endpoint_id],
                                    online_count: 1,
                                };
                                // Non-blocking — drop events if the receiver is full.
                                let _ = tx.try_send(mesh);
                            }
                        }
                        DiscoveryEvent::Expired { endpoint_id } => {
                            debug!(peer = %endpoint_id, "mDNS: peer expired");
                        }
                    }
                }
            });
            Some(rx)
        } else {
            None
        }
    };

    // Resolve device name for pairing: system hostname or a safe fallback.
    let device_name = {
        let hostname = gethostname::gethostname();
        hostname.to_str().unwrap_or("Sync Daemon").to_string()
    };
    info!(device_name = %device_name, "Device name resolved for pairing");

    // Extract the file event receiver from the watcher for injection into Daemon.
    let (file_event_rx, _watcher) = watcher.into_event_rx();

    let mut daemon = Daemon::new(
        Arc::new(Mutex::new(vault)),
        sync_node,
        vault_gossip,
        file_event_rx,
        discovery_rx,
        allowlist,
        device_name,
        relay_url.as_ref().map(|u| u.to_string()),
        config.vault.clone(),
        shutdown,
    );

    info!("Daemon running. Press Ctrl+C to stop.");

    daemon.run_loop().await;

    // Graceful shutdown
    if let Err(e) = daemon.sync_node.shutdown().await {
        warn!("Error during iroh node shutdown: {}", e);
    }
    if let Some(relay) = embedded_relay {
        relay.shutdown().await;
        // Clear the relay URL from daemon.toml so the plugin doesn't try to use a dead relay.
        if let Err(e) = daemon_config.set_relay_url(None, &config.vault) {
            warn!("Failed to clear relay URL from daemon.toml: {}", e);
        }
    }

    Ok(())
}
