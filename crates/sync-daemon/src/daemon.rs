//! Daemon event loop extracted as a library entry point.
//!
//! This allows the `memory` binary to embed the sync daemon behind a feature
//! flag, while still keeping the standalone `sync-daemon` binary as a thin wrapper.

use anyhow::{Context, Result};
use iroh::EndpointId;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::{broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{debug, error, info, warn};
use crate::allowlist::FileAllowlistStorage;
use crate::daemon_lock::DaemonLock;
use crate::http;
use crate::native_fs::NativeFs;
use crate::pair_api::{
    ConnectionState, DaemonCommand, DaemonControl, DaemonStatus, PairingUiEvent, PeerSummary,
    PAIRING_BROADCAST_CAPACITY,
};
use crate::persistence::DaemonConfig;
use crate::relay::EmbeddedRelay;
use crate::watcher::{FileEvent, FileEventKind, FileWatcher};

use sync_core::allowlist::{AllowedPeer, AllowlistStorage};
use sync_core::fs::FileSystem;
use sync_core::network::{
    discovery::MeshMetadata,
    gossip::{GossipEvent, VaultGossip},
    pairing::{InboundPairingExchange, PairingApproval, PairingEvent},
    streams::InboundSyncRequest,
    SyncNode,
};
use sync_core::pairing::{PairingChallenge, PairingSession};
use sync_core::{PeerId, PeerRegistry, Vault};

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
    #[allow(dead_code)]
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
            DaemonCommand::StartDiscovery { reply: _ } => {
                // Placeholder: full mDNS-scan-to-channel wiring is implemented in
                // commit 4 (initiator window). For now, accept the command silently
                // so the type system is satisfied and Wave B can build on top.
                debug!("DaemonCommand::StartDiscovery received (not yet implemented)");
            }
            DaemonCommand::SubmitCode { code: _, reply } => {
                // Placeholder: wired in commit 4 (initiator window).
                debug!("DaemonCommand::SubmitCode received (not yet implemented)");
                let _ = reply.send(Err("not yet implemented".to_string()));
            }
            DaemonCommand::CancelInitiate { reply } => {
                // Placeholder: wired in commit 4 (initiator window).
                debug!("DaemonCommand::CancelInitiate received (not yet implemented)");
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
            info!("Deleted {} from registry tree (no peers to broadcast)", path);
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
        self.peer_registry.lock().await.on_neighbor_up(peer_id.clone());
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
                        warn!("Failed to update allowlist last_seen for {}: {}", node_id, e);
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

        match self.allowlist.add_peer(peer.node_id, &peer.device_name).await {
            Ok(()) => {
                info!(peer_id = %peer.node_id, device = %peer.device_name, "Added peer via gossip allowlist update");
            }
            Err(e) => {
                error!("Failed to add peer {} from allowlist update: {}", peer.node_id, e);
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
            code: self.active_pairing.as_ref().map(|s| s.code.clone()).unwrap_or_default(),
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
        let is_first_pair = matches!(self.allowlist.list_peers().await, Ok(peers) if peers.is_empty());

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
        if let Err(e) = self.vault_gossip.broadcast_allowlist_update(&allowed_peer).await {
            warn!("Failed to broadcast allowlist update for {}: {}", device_name, e);
        }

        info!("Device '{}' joined the mesh", device_name);

        self.emit_pairing_event(PairingUiEvent::InboundCompleted {
            device_name: device_name.clone(),
        });
        self.emit_status().await;
    }

    /// Handle a failed pairing attempt.
    fn on_pairing_failed(&mut self, peer_id: PeerId, reason: String) {
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

                self.peer_registry.lock().await.update_last_seen(&inbound.remote_id);
                if let Err(e) = self.allowlist.update_last_seen(&inbound.remote_id, now_ms()).await {
                    warn!("Failed to update allowlist last_seen for {}: {}", inbound.remote_id, e);
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

    let (mut daemon, embedded_relay, mut daemon_config, vault_path, mesh_name) = startup_result?;

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

/// Startup phase: lock acquisition, vault init, node startup.
///
/// Returns the fully-initialized `Daemon` plus the components needed for
/// graceful shutdown (`EmbeddedRelay`, `DaemonConfig`, vault path, mesh name).
/// All startup failures surface as `Err` before the daemon is returned, so
/// callers are guaranteed to receive a fully-operational `Daemon`.
///
/// Used by `run_with_shutdown_controlled`; the event loop is run by the caller.
async fn startup_inner(
    config: &DaemonRunConfig,
    shutdown: CancellationToken,
) -> Result<(Daemon<NativeFs, FileAllowlistStorage>, Option<EmbeddedRelay>, DaemonConfig, PathBuf, String)> {
    // Acquire exclusive daemon lock
    let _daemon_lock = DaemonLock::acquire(&config.vault)
        .context("Failed to acquire daemon lock — is another daemon already running on this vault?")?;

    info!("Starting sync daemon");
    info!("Vault path: {:?}", config.vault);

    let fs = NativeFs::new(config.vault.clone());

    let vault = if fs.exists(".sync").await? {
        info!("Loading existing vault");
        Vault::load(fs).await?
    } else {
        info!("Initializing new vault");
        Vault::init(fs).await?
    };

    let vault_id = vault.vault_id();
    info!("Vault loaded, vault ID: {}", vault_id);

    let (mut daemon_config, identity_key) = DaemonConfig::load_or_generate(
        &config.vault,
        config.identity_key.as_deref(),
    )
    .await?;

    info!("Daemon PeerId: {}", daemon_config.peer_id);

    if daemon_config.relay_url.is_some() {
        info!("Clearing stale relay URL from previous run");
        if let Err(e) = daemon_config.set_relay_url(None, &config.vault) {
            warn!("Failed to clear stale relay URL: {}", e);
        }
    }

    let embedded_relay: Option<EmbeddedRelay> = if let Some(ref addr_str) = config.relay_listen {
        match addr_str.parse() {
            Ok(bind_addr) => {
                match EmbeddedRelay::start(bind_addr).await {
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
                warn!("Invalid relay-listen address '{}': {}, continuing without relay", addr_str, e);
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
        config.vault
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
                    .map_err(|e| warn!("Skipping invalid allowlist peer for gossip bootstrap: {}", e))
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
        info!("Health endpoint started on {}", config.health_listen.as_ref().unwrap());
    }

    let watcher = FileWatcher::new(config.vault.clone())?;
    info!("File watcher started");

    let discovery_rx: Option<tokio::sync::mpsc::Receiver<sync_core::network::discovery::DiscoveredMesh>> = {
        use futures::StreamExt;
        use sync_core::network::discovery::{DiscoveredMesh, MeshMetadata as DiscoveryMeshMetadata};
        use iroh::address_lookup::DiscoveryEvent;

        if let Some(stream) = sync_node.subscribe_discovery().await {
            let (tx, rx) = tokio::sync::mpsc::channel(16);
            tokio::spawn(async move {
                futures::pin_mut!(stream);
                while let Some(event) = stream.next().await {
                    match event {
                        DiscoveryEvent::Discovered { endpoint_info, .. } => {
                            let metadata = endpoint_info
                                .data
                                .user_data()
                                .and_then(|ud| serde_json::from_str::<DiscoveryMeshMetadata>(ud.as_ref()).ok());

                            if let Some(meta) = metadata {
                                let mesh = DiscoveredMesh {
                                    mesh_name: meta.mesh.clone(),
                                    vault_id: meta.vid.clone(),
                                    peers: vec![endpoint_info.endpoint_id],
                                    online_count: 1,
                                };
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
        hostname
            .to_str()
            .unwrap_or("Sync Daemon")
            .to_string()
    };
    info!(device_name = %device_name, "Device name resolved for pairing");

    let (file_event_rx, _watcher) = watcher.into_event_rx();

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

    Ok((daemon, embedded_relay, daemon_config, config.vault.clone(), mesh_name))
}

/// Inner daemon body: lock acquisition, vault init, node startup, event loop,
/// and graceful shutdown. Called by `run_with_shutdown`.
async fn run_inner(config: DaemonRunConfig, shutdown: CancellationToken) -> Result<()> {
    // Acquire exclusive daemon lock
    let _daemon_lock = DaemonLock::acquire(&config.vault)
        .context("Failed to acquire daemon lock — is another daemon already running on this vault?")?;

    info!("Starting sync daemon");
    info!("Vault path: {:?}", config.vault);

    let fs = NativeFs::new(config.vault.clone());

    let vault = if fs.exists(".sync").await? {
        info!("Loading existing vault");
        Vault::load(fs).await?
    } else {
        info!("Initializing new vault");
        Vault::init(fs).await?
    };

    let vault_id = vault.vault_id();
    info!("Vault loaded, vault ID: {}", vault_id);

    let (mut daemon_config, identity_key) = DaemonConfig::load_or_generate(
        &config.vault,
        config.identity_key.as_deref(),
    )
    .await?;

    info!("Daemon PeerId: {}", daemon_config.peer_id);

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
            Ok(bind_addr) => {
                match EmbeddedRelay::start(bind_addr).await {
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
                warn!("Invalid relay-listen address '{}': {}, continuing without relay", addr_str, e);
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
        config.vault
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
                    .map_err(|e| warn!("Skipping invalid allowlist peer for gossip bootstrap: {}", e))
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
        info!("Health endpoint started on {}", config.health_listen.as_ref().unwrap());
    }

    let watcher = FileWatcher::new(config.vault.clone())?;
    info!("File watcher started");

    // Spawn mDNS discovery events into a channel so the main select loop stays uniform.
    // Discovery is native-only; the channel is None when mDNS is unavailable.
    let discovery_rx: Option<tokio::sync::mpsc::Receiver<sync_core::network::discovery::DiscoveredMesh>> = {
        use futures::StreamExt;
        use sync_core::network::discovery::{DiscoveredMesh, MeshMetadata as DiscoveryMeshMetadata};
        use iroh::address_lookup::DiscoveryEvent;

        if let Some(stream) = sync_node.subscribe_discovery().await {
            let (tx, rx) = tokio::sync::mpsc::channel(16);
            tokio::spawn(async move {
                futures::pin_mut!(stream);
                while let Some(event) = stream.next().await {
                    match event {
                        DiscoveryEvent::Discovered { endpoint_info, .. } => {
                            let metadata = endpoint_info
                                .data
                                .user_data()
                                .and_then(|ud| serde_json::from_str::<DiscoveryMeshMetadata>(ud.as_ref()).ok());

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
        hostname
            .to_str()
            .unwrap_or("Sync Daemon")
            .to_string()
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
