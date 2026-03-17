//! Daemon event loop extracted as a library entry point.
//!
//! This allows the `memory` binary to embed the sync daemon behind a feature
//! flag, while still keeping the standalone `sync-daemon` binary as a thin wrapper.

use anyhow::{Context, Result};
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{mpsc, Mutex};
use tracing::{debug, error, info};

use crate::daemon_lock::DaemonLock;
use crate::http;
use crate::manager::{ConnectionManager, ManagerEvent};
use crate::native_fs::NativeFs;
use crate::persistence::DaemonConfig;
use crate::server::{ServerEvent, WebSocketServer};
use crate::watcher::{FileEvent, FileEventKind, FileWatcher};
use crate::IncomingMessage;

use sync_core::fs::FileSystem;
use sync_core::{PeerId, Vault};

/// Configuration for running the sync daemon.
pub struct DaemonRunConfig {
    pub vault: PathBuf,
    pub listen: String,
    pub advertise: Option<String>,
    pub bootstrap: Vec<String>,
    pub client_only: bool,
    /// Optional path to an alternate identity key file (replaces `--peer-id` flag).
    /// If None, the default `.sync/daemon.key` is used.
    pub identity_key: Option<PathBuf>,
}

/// Daemon state holding all components.
struct Daemon {
    vault: Arc<Mutex<Vault<NativeFs>>>,
    server: WebSocketServer,
    outgoing: ConnectionManager,
    watcher: FileWatcher,
    /// Currently connected peer IDs.
    connected_peers: HashSet<PeerId>,
    vault_path: PathBuf,
}

impl Daemon {
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

    /// Handle a file deletion.
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

        if self.server.peer_count() > 0 {
            match vault.prepare_file_deleted(path) {
                Ok(msg) => {
                    drop(vault);
                    self.server.broadcast(&msg).await;
                    info!(
                        "Broadcast deletion of {} to {} peer(s)",
                        path,
                        self.server.peer_count()
                    );
                }
                Err(e) => {
                    error!("Failed to prepare deletion message for {}: {}", path, e);
                }
            }
        } else {
            info!(
                "Deleted {} from registry tree (no peers to broadcast)",
                path
            );
        }
    }

    /// Handle a file modification.
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

        if self.server.peer_count() == 0 {
            return;
        }

        match vault.prepare_document_update(path).await {
            Ok(Some(update)) => {
                drop(vault);
                self.server.broadcast(&update).await;
                info!(
                    "Broadcast update for {} to {} peer(s)",
                    path,
                    self.server.peer_count()
                );
            }
            Ok(None) => {
                debug!("No update to broadcast for {}", path);
            }
            Err(e) => {
                error!("Failed to prepare update for {}: {}", path, e);
            }
        }
    }

    /// Handle a sync message from a peer.
    async fn on_sync_message(&mut self, msg: IncomingMessage) {
        let peer_id = &msg.peer_id;

        debug!(
            "Processing message from {} ({} bytes)",
            peer_id,
            msg.data.len()
        );

        let should_relay_raw = self.is_file_lifecycle_message(&msg.data);

        let vault = self.vault.lock().await;

        match vault.process_sync_message(&msg.data).await {
            Ok((response, modified_paths)) => {
                if let Some(response_data) = response {
                    if let Err(e) = self.server.send(peer_id, &response_data).await {
                        error!("Failed to send sync response to {}: {}", peer_id, e);
                    }
                }

                if !modified_paths.is_empty() && self.server.peer_count() > 1 {
                    if should_relay_raw {
                        self.server
                            .broadcast_except(&msg.data, peer_id)
                            .await;
                        info!(
                            "Relayed file lifecycle event for {} to {} other peer(s)",
                            modified_paths.join(", "),
                            self.server.peer_count() - 1
                        );
                    } else {
                        for path in &modified_paths {
                            match vault.prepare_document_update(path).await {
                                Ok(Some(update)) => {
                                    self.server.broadcast_except(&update, peer_id).await;
                                }
                                Ok(None) => {
                                    debug!("No update to relay for {}", path);
                                }
                                Err(e) => {
                                    error!(
                                        "Failed to prepare relay update for {}: {}",
                                        path, e
                                    );
                                }
                            }
                        }
                        info!(
                            "Relayed {} file(s) from {} to {} other peer(s)",
                            modified_paths.len(),
                            peer_id,
                            self.server.peer_count() - 1
                        );
                    }
                }

                drop(vault);

                if !modified_paths.is_empty() {
                    info!("Synced {} file(s) from {}", modified_paths.len(), peer_id);
                }
            }
            Err(e) => {
                error!("Failed to process sync message from {}: {}", peer_id, e);
            }
        }
    }

    fn is_file_lifecycle_message(&self, data: &[u8]) -> bool {
        let msg: Result<sync_core::SyncMessage, _> = bincode::deserialize(data);
        matches!(
            msg,
            Ok(sync_core::SyncMessage::FileDeleted { .. })
                | Ok(sync_core::SyncMessage::FileRenamed { .. })
        )
    }

    /// Handle a newly connected peer (after handshake).
    async fn on_peer_connected(&mut self, peer_id: String, _address: Option<String>) {
        info!("Peer connected: {}", peer_id);

        if let Ok(pid) = peer_id.parse::<PeerId>() {
            self.connected_peers.insert(pid);
        }

        let vault = self.vault.lock().await;
        match vault.prepare_sync_request().await {
            Ok(request) => {
                drop(vault);
                if let Err(e) = self.server.send(&peer_id, &request).await {
                    error!("Failed to send sync request to {}: {}", peer_id, e);
                } else {
                    debug!("Sent sync request to {}", peer_id);
                }
            }
            Err(e) => {
                error!("Failed to prepare sync request for {}: {}", peer_id, e);
            }
        }
    }

    fn on_peer_disconnected(&mut self, peer_id: &str) {
        if let Ok(pid) = peer_id.parse::<PeerId>() {
            self.connected_peers.remove(&pid);
        }
        debug!("Peer disconnected: {}", peer_id);
    }
}

/// Run the sync daemon with the given configuration.
///
/// This is the main entry point for embedding the daemon in the `memory` binary.
/// Assumes logging is already configured by the caller.
pub async fn run(config: DaemonRunConfig) -> Result<()> {
    // Acquire exclusive daemon lock
    let _daemon_lock = DaemonLock::acquire(&config.vault)
        .context("Failed to acquire daemon lock — is another daemon already running on this vault?")?;

    info!("Starting sync daemon");
    info!("Vault path: {:?}", config.vault);
    if !config.client_only {
        info!("Listen address: {}", config.listen);
    }
    if let Some(ref advertise) = config.advertise {
        info!("Advertised address: {}", advertise);
    }
    if config.client_only {
        info!("Running in client-only mode (no incoming connections)");
    }

    let fs = NativeFs::new(config.vault.clone());

    let vault = if fs.exists(".sync").await? {
        info!("Loading existing vault");
        Vault::load(fs).await?
    } else {
        info!("Initializing new vault");
        Vault::init(fs).await?
    };

    info!("Vault loaded, vault ID: {}", vault.vault_id());

    let daemon_config = DaemonConfig::load_or_generate(
        &config.vault,
        config.identity_key.as_deref(),
    )
    .await?;
    let peer_id = daemon_config.peer_id;

    let server = WebSocketServer::new(peer_id.to_string(), config.advertise.clone());

    let (outgoing, mut outgoing_rx) =
        ConnectionManager::new(peer_id.to_string(), config.advertise.clone());

    let (ws_tx, mut ws_rx) = mpsc::channel(32);
    if !config.client_only {
        let listen_addr = config.listen.clone();
        tokio::spawn(async move {
            http::serve(&listen_addr, ws_tx).await;
        });
    }

    let watcher = FileWatcher::new(config.vault.clone())?;
    info!("File watcher started");

    let mut daemon = Daemon {
        vault: Arc::new(Mutex::new(vault)),
        server,
        outgoing,
        watcher,
        connected_peers: HashSet::new(),
        vault_path: config.vault.clone(),
    };

    for bootstrap_addr in &config.bootstrap {
        info!("Connecting to bootstrap peer: {}", bootstrap_addr);
        if let Err(e) = daemon.outgoing.connect_to(bootstrap_addr).await {
            error!(
                "Failed to connect to bootstrap peer {}: {}",
                bootstrap_addr, e
            );
        }
    }

    info!("Daemon running. Press Ctrl+C to stop.");

    loop {
        tokio::select! {
            Some((ws, addr)) = ws_rx.recv() => {
                daemon.server.accept_connection(ws, addr).await;
            }

            Some(event) = daemon.watcher.event_rx().recv() => {
                daemon.on_file_changed(event).await;
            }

            Some(event) = daemon.server.poll_event() => {
                match event {
                    ServerEvent::PeerConnected { peer_id, address } => {
                        daemon.on_peer_connected(peer_id, address).await;
                    }
                    ServerEvent::Message(msg) => {
                        daemon.on_sync_message(msg).await;
                    }
                    ServerEvent::PeerDisconnected { peer_id } => {
                        info!("Peer disconnected: {}", peer_id);
                        daemon.on_peer_disconnected(&peer_id);
                    }
                }
            }

            Some(event) = outgoing_rx.recv() => {
                match event {
                    ManagerEvent::Message(msg) => {
                        daemon.on_sync_message(msg).await;
                    }
                    ManagerEvent::HandshakeComplete { peer_id, address, .. } => {
                        info!("Outgoing connection established to {}", peer_id);
                        daemon.on_peer_connected(peer_id, address).await;
                    }
                    ManagerEvent::ConnectionClosed { peer_id, reason } => {
                        info!("Outgoing connection closed: {} ({:?})", peer_id, reason);
                        daemon.on_peer_disconnected(&peer_id);
                    }
                    ManagerEvent::PeerDiscovered { peer_id, address } => {
                        info!("Discovered peer {} at {}", peer_id, address);
                    }
                }
            }

            _ = memory_common::shutdown_signal() => {
                info!("Shutting down");
                break;
            }
        }
    }

    Ok(())
}
