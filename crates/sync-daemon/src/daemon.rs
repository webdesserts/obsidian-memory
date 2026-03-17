//! Daemon event loop extracted as a library entry point.
//!
//! This allows the `memory` binary to embed the sync daemon behind a feature
//! flag, while still keeping the standalone `sync-daemon` binary as a thin wrapper.

use anyhow::{Context, Result};
use iroh::EndpointId;
use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};

use crate::daemon_lock::DaemonLock;
use crate::http;
use crate::native_fs::NativeFs;
use crate::persistence::DaemonConfig;
use crate::relay::EmbeddedRelay;
use crate::watcher::{FileEvent, FileEventKind, FileWatcher};

use sync_core::fs::FileSystem;
use sync_core::network::{
    gossip::{GossipEvent, VaultGossip},
    streams::InboundSyncRequest,
    SyncNode,
};
use sync_core::Vault;

/// Configuration for running the sync daemon.
pub struct DaemonRunConfig {
    pub vault: PathBuf,
    /// Optional path to an alternate identity key file (default: `.sync/daemon.key`).
    pub identity_key: Option<PathBuf>,
    /// Bootstrap peer EndpointId hex strings to join the gossip swarm.
    pub bootstrap_peers: Vec<String>,
    /// If set, serve a `/health` endpoint on this address (e.g. `"127.0.0.1:8081"`).
    pub health_listen: Option<String>,
    /// If set, start an embedded iroh relay server on this address (e.g. `"0.0.0.0:3340"`).
    ///
    /// Relay startup failure is non-fatal — the daemon will log a warning and continue
    /// without relay support rather than refusing to start.
    pub relay_listen: Option<String>,
}

/// Daemon state holding all components.
struct Daemon {
    vault: Arc<Mutex<Vault<NativeFs>>>,
    sync_node: SyncNode,
    vault_gossip: VaultGossip,
    watcher: FileWatcher,
    /// EndpointIds of peers currently in the gossip swarm for this vault.
    connected_peers: HashSet<EndpointId>,
    #[allow(dead_code)]
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

        if !self.connected_peers.is_empty() {
            if let Err(e) = self.vault_gossip.broadcast_change(path).await {
                error!("Failed to broadcast deletion of {}: {}", path, e);
            } else {
                info!(
                    "Broadcast deletion of {} to {} peer(s)",
                    path,
                    self.connected_peers.len()
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

        if self.connected_peers.is_empty() {
            return;
        }

        // Notify peers via gossip; they will open a QUIC stream to pull the full update.
        if let Err(e) = self.vault_gossip.broadcast_change(path).await {
            error!("Failed to broadcast change for {}: {}", path, e);
        } else {
            info!(
                "Broadcast change for {} to {} peer(s)",
                path,
                self.connected_peers.len()
            );
        }
    }

    /// A peer joined the gossip swarm — initiate a full sync via QUIC.
    async fn on_neighbor_up(&mut self, node_id: EndpointId) {
        info!(peer = %node_id, "Gossip NeighborUp — initiating full sync");
        self.connected_peers.insert(node_id);

        let vault = self.vault.lock().await;
        let request_bytes = match vault.prepare_sync_request().await {
            Ok(bytes) => bytes,
            Err(e) => {
                error!("Failed to prepare sync request for {}: {}", node_id, e);
                return;
            }
        };
        drop(vault);

        let request: sync_core::SyncMessage = match bincode::deserialize(&request_bytes) {
            Ok(msg) => msg,
            Err(e) => {
                error!("Failed to deserialize prepared sync request: {}", e);
                return;
            }
        };

        let response = match sync_core::network::streams::connect_and_sync(
            &self.sync_node.endpoint,
            node_id,
            request,
        )
        .await
        {
            Ok(resp) => resp,
            Err(e) => {
                // NeighborUp fires before the peer's QUIC listener is ready on some
                // OS configurations. Log a warning and move on — the peer will initiate
                // a sync in the other direction.
                warn!("Failed to connect for sync with {}: {}", node_id, e);
                return;
            }
        };

        let response_bytes = match bincode::serialize(&response) {
            Ok(bytes) => bytes,
            Err(e) => {
                error!("Failed to serialize sync response: {}", e);
                return;
            }
        };

        let vault = self.vault.lock().await;
        match vault.process_sync_message(&response_bytes).await {
            Ok((_, modified_paths)) => {
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
    }

    /// A peer left the gossip swarm.
    fn on_neighbor_down(&mut self, node_id: EndpointId) {
        info!(peer = %node_id, "Gossip NeighborDown");
        self.connected_peers.remove(&node_id);
    }

    /// A change notification arrived via gossip — pull the changed file via QUIC.
    async fn on_change_received(&mut self, from: EndpointId, path: String) {
        debug!(peer = %from, path = %path, "Change notification received — pulling update");

        let vault = self.vault.lock().await;
        let request_bytes = match vault.prepare_sync_request().await {
            Ok(bytes) => bytes,
            Err(e) => {
                error!("Failed to prepare sync request for change pull: {}", e);
                return;
            }
        };
        drop(vault);

        let request: sync_core::SyncMessage = match bincode::deserialize(&request_bytes) {
            Ok(msg) => msg,
            Err(e) => {
                error!("Failed to deserialize prepared sync request: {}", e);
                return;
            }
        };

        let response = match sync_core::network::streams::connect_and_sync(
            &self.sync_node.endpoint,
            from,
            request,
        )
        .await
        {
            Ok(resp) => resp,
            Err(e) => {
                warn!("Failed to pull change from {}: {}", from, e);
                return;
            }
        };

        let response_bytes = match bincode::serialize(&response) {
            Ok(bytes) => bytes,
            Err(e) => {
                error!("Failed to serialize sync response: {}", e);
                return;
            }
        };

        let vault = self.vault.lock().await;
        match vault.process_sync_message(&response_bytes).await {
            Ok((_, modified_paths)) => {
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
    }

    /// Handle an inbound sync request from a remote peer (via QUIC bi-stream).
    async fn on_inbound_sync(&self, inbound: InboundSyncRequest) {
        let msg_bytes = match bincode::serialize(&inbound.message) {
            Ok(bytes) => bytes,
            Err(e) => {
                error!("Failed to serialize inbound sync message: {}", e);
                return;
            }
        };

        let vault = self.vault.lock().await;
        match vault.process_sync_message(&msg_bytes).await {
            Ok((response_bytes_opt, modified_paths)) => {
                if !modified_paths.is_empty() {
                    info!("Applied {} file(s) from inbound sync", modified_paths.len());
                }

                if let Some(resp_bytes) = response_bytes_opt {
                    let response: sync_core::SyncMessage = match bincode::deserialize(&resp_bytes) {
                        Ok(msg) => msg,
                        Err(e) => {
                            error!("Failed to deserialize sync response: {}", e);
                            return;
                        }
                    };
                    // reply_tx being dropped closes the stream without a response — that's fine.
                    let _ = inbound.reply_tx.send(response);
                }
            }
            Err(e) => {
                error!("Failed to process inbound sync message: {}", e);
            }
        }
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
    let sync_node = SyncNode::new(secret_key_bytes, relay_url.as_ref())
        .await
        .context("Failed to create iroh SyncNode")?;

    info!(node_id = %sync_node.node_id(), "Iroh node started");

    // Parse bootstrap peers as EndpointId (iroh node ID hex strings)
    let bootstrap_ids: Vec<EndpointId> = config
        .bootstrap_peers
        .iter()
        .filter_map(|hex| {
            hex.parse::<EndpointId>()
                .map_err(|e| {
                    error!("Invalid bootstrap peer ID '{}': {}", hex, e);
                })
                .ok()
        })
        .collect();

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

    let mut daemon = Daemon {
        vault: Arc::new(Mutex::new(vault)),
        sync_node,
        vault_gossip,
        watcher,
        connected_peers: HashSet::new(),
        vault_path: config.vault.clone(),
    };

    info!("Daemon running. Press Ctrl+C to stop.");

    loop {
        tokio::select! {
            Some(event) = daemon.watcher.event_rx().recv() => {
                daemon.on_file_changed(event).await;
            }

            Some(gossip_event) = daemon.vault_gossip.event_rx.recv() => {
                match gossip_event {
                    GossipEvent::NeighborUp(node_id) => {
                        daemon.on_neighbor_up(node_id).await;
                    }
                    GossipEvent::NeighborDown(node_id) => {
                        daemon.on_neighbor_down(node_id);
                    }
                    GossipEvent::ChangeReceived { from, notification } => {
                        daemon.on_change_received(from, notification.path).await;
                    }
                }
            }

            Some(inbound) = daemon.sync_node.inbound_sync_rx.recv() => {
                daemon.on_inbound_sync(inbound).await;
            }

            _ = memory_common::shutdown_signal() => {
                info!("Shutting down");
                break;
            }
        }
    }

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
