//! sync-daemon: Headless P2P sync daemon for home server.
//!
//! Uses the same sync-core as the Obsidian plugin, but runs as a native binary
//! with native filesystem and networking.

use anyhow::{Context, Result};
use clap::Parser;
use serde_json::Value;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn};
use tracing_subscriber::EnvFilter;

// Use library exports
use sync_daemon::connection::ConnectionEvent;
use sync_daemon::manager::{ConnectionManager, ManagerEvent};
use sync_daemon::native_fs::NativeFs;
use sync_daemon::server::WebSocketServer;
use sync_daemon::watcher::{FileEvent, FileEventKind, FileWatcher};
use sync_daemon::IncomingMessage;

use sync_core::fs::FileSystem;
use sync_core::swim::{GossipUpdate, MembershipList, PeerInfo};
use sync_core::{PeerId, Vault};

#[derive(Parser, Debug)]
#[command(name = "sync-daemon")]
#[command(about = "P2P vault sync daemon")]
struct Args {
    #[command(subcommand)]
    command: Option<Command>,

    /// Path to the vault directory
    #[arg(short, long)]
    vault: PathBuf,

    /// Address to listen on for incoming connections
    #[arg(short, long, default_value = "0.0.0.0:8080")]
    listen: String,

    /// Address to advertise to other peers (how they connect back to us)
    /// Example: ws://my.server.com:8080
    #[arg(long)]
    advertise: Option<String>,

    /// Bootstrap peer(s) to connect to on startup
    /// Can be specified multiple times
    #[arg(long)]
    bootstrap: Vec<String>,

    /// Run in client-only mode (don't listen for incoming connections)
    #[arg(long)]
    client_only: bool,

    /// Peer ID (generated if not provided)
    #[arg(long)]
    peer_id: Option<String>,

    /// Enable verbose logging
    #[arg(long)]
    verbose: bool,
}

#[derive(clap::Subcommand, Debug)]
enum Command {
    /// Connect to a peer and add them to the mesh
    AddPeer {
        /// WebSocket address of the peer (e.g., ws://peer.example.com:8080)
        address: String,
    },
}

/// Parsed JSON message from plugin.
enum JsonMessage {
    /// Pure gossip message
    Gossip(Vec<GossipUpdate>),
    /// Sync message with optional piggybacked gossip
    Sync {
        data: Vec<u8>,
        gossip: Vec<GossipUpdate>,
    },
}

/// Daemon state holding all components.
struct Daemon {
    /// The sync vault (behind mutex for async access)
    vault: Arc<Mutex<Vault<NativeFs>>>,
    /// WebSocket server (for incoming connections)
    server: WebSocketServer,
    /// Connection manager (for outgoing connections)
    outgoing: ConnectionManager,
    /// File watcher
    watcher: FileWatcher,
    /// SWIM membership list for gossip-based peer discovery
    membership: MembershipList,
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

        // Check if this deletion was from sync (consume flag)
        if vault.consume_sync_flag(path) {
            debug!("Skipping broadcast for synced deletion: {}", path);
            return;
        }

        // Delete file from tree (CRDT operation)
        if let Err(e) = vault.delete_file(path).await {
            error!("Failed to delete file {}: {}", path, e);
            return;
        }

        // Broadcast deletion to peers
        if self.server.peer_count() > 0 {
            match vault.prepare_file_deleted(path) {
                Ok(msg) => {
                    drop(vault); // Release lock before network I/O
                    self.server.broadcast(&msg).await;
                    info!("Broadcast deletion of {} to {} peer(s)", path, self.server.peer_count());
                }
                Err(e) => {
                    error!("Failed to prepare deletion message for {}: {}", path, e);
                }
            }
        } else {
            info!("Deleted {} from registry tree (no peers to broadcast)", path);
        }
    }

    /// Handle a file modification.
    async fn on_file_modified(&mut self, path: &str) {
        // Skip broadcast if no peers connected
        if self.server.peer_count() == 0 {
            return;
        }

        let vault = self.vault.lock().await;

        // Check if this modification was from sync (consume flag)
        if vault.consume_sync_flag(path) {
            debug!("Skipping broadcast for synced file: {}", path);
            return;
        }

        // Notify vault of the file change
        if let Err(e) = vault.on_file_changed(path).await {
            error!("Failed to process file change for {}: {}", path, e);
            return;
        }

        // Prepare and broadcast document update
        match vault.prepare_document_update(path).await {
            Ok(Some(update)) => {
                drop(vault); // Release lock before network I/O
                self.server.broadcast(&update).await;
                info!("Broadcast update for {} to {} peer(s)", path, self.server.peer_count());
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
        let peer_id = self
            .server
            .resolve_peer_id(&msg.temp_id)
            .unwrap_or_else(|| msg.temp_id.clone());

        debug!("Processing message from {} ({} bytes)", peer_id, msg.data.len());

        // Try to parse as JSON first (handles gossip, sync-with-gossip, etc.)
        let sync_data = match Self::try_parse_json_message(&msg.data) {
            Some(JsonMessage::Gossip(updates)) => {
                // Pure gossip message - process and relay
                self.handle_gossip_updates(&updates, &peer_id, &msg.temp_id).await;
                return;
            }
            Some(JsonMessage::Sync { data, gossip }) => {
                // Sync message with optional piggybacked gossip
                if !gossip.is_empty() {
                    self.handle_gossip_updates(&gossip, &peer_id, &msg.temp_id).await;
                }
                data
            }
            None => {
                // Not JSON - treat as raw binary sync message
                msg.data.clone()
            }
        };

        // Check if this is a FileDeleted or FileRenamed message that should be relayed directly
        let should_relay_raw = self.is_file_lifecycle_message(&sync_data);

        let vault = self.vault.lock().await;

        match vault.process_sync_message(&sync_data).await {
            Ok((response, modified_paths)) => {
                // Send response if any
                if let Some(response_data) = response {
                    if let Err(e) = self.server.send_by_temp_id(&msg.temp_id, &response_data).await {
                        error!("Failed to send sync response to {}: {}", peer_id, e);
                    }
                }

                // Relay to OTHER peers (not the sender)
                if !modified_paths.is_empty() && self.server.peer_count() > 1 {
                    if should_relay_raw {
                        // FileDeleted/FileRenamed: relay the original message directly
                        self.server.broadcast_except(&sync_data, &msg.temp_id).await;
                        info!(
                            "Relayed file lifecycle event for {} to {} other peer(s)",
                            modified_paths.join(", "),
                            self.server.peer_count() - 1
                        );
                    } else {
                        // DocumentUpdate or other: prepare fresh updates
                        for path in &modified_paths {
                            match vault.prepare_document_update(path).await {
                                Ok(Some(update)) => {
                                    self.server.broadcast_except(&update, &msg.temp_id).await;
                                }
                                Ok(None) => {
                                    debug!("No update to relay for {}", path);
                                }
                                Err(e) => {
                                    error!("Failed to prepare relay update for {}: {}", path, e);
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

                drop(vault); // Release lock after all operations

                if !modified_paths.is_empty() {
                    info!("Synced {} file(s) from {}", modified_paths.len(), peer_id);
                }
            }
            Err(e) => {
                error!("Failed to process sync message from {}: {}", peer_id, e);
            }
        }
    }

    /// Check if a message is a FileDeleted or FileRenamed (should be relayed directly)
    fn is_file_lifecycle_message(&self, data: &[u8]) -> bool {
        // Deserialize to check the variant type safely (don't rely on bincode internals)
        let msg: Result<sync_core::SyncMessage, _> = bincode::deserialize(data);
        matches!(
            msg,
            Ok(sync_core::SyncMessage::FileDeleted { .. })
                | Ok(sync_core::SyncMessage::FileRenamed { .. })
        )
    }

    /// Handle a newly connected peer (after handshake).
    async fn on_peer_connected(&mut self, peer_id: String, address: Option<String>) {
        info!("Peer connected: {}", peer_id);

        // Add peer to SWIM membership
        if let Ok(pid) = peer_id.parse::<PeerId>() {
            let peer_info = PeerInfo::new(pid, address);
            self.membership.add(peer_info.clone(), 1);

            // Send full gossip to the new peer
            let full_gossip = self.membership.generate_full_gossip();
            let gossip_msg = serde_json::json!({ "type": "gossip", "updates": full_gossip });
            if let Err(e) = self
                .server
                .send(&peer_id, gossip_msg.to_string().as_bytes())
                .await
            {
                warn!("Failed to send gossip to {}: {}", peer_id, e);
            } else {
                debug!("Sent full gossip ({} updates) to {}", full_gossip.len(), peer_id);
            }

            // Broadcast the new peer to existing peers
            let alive_update = GossipUpdate::alive(peer_info, 1);
            let broadcast_msg = serde_json::json!({ "type": "gossip", "updates": [alive_update] });
            self.server
                .broadcast_except(broadcast_msg.to_string().as_bytes(), &peer_id)
                .await;
        }

        // Send sync request
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

    /// Handle peer disconnection.
    fn on_peer_disconnected(&mut self, peer_id: &str) {
        if let Ok(pid) = peer_id.parse::<PeerId>() {
            if self.membership.mark_dead(pid) {
                debug!("Marked {} as Dead in SWIM membership", peer_id);
            }
        }
    }

    /// Broadcast dead gossip for a disconnected peer.
    async fn broadcast_dead_gossip(&mut self, peer_id: &str) {
        if let Ok(pid) = peer_id.parse::<PeerId>() {
            if let Some(member) = self.membership.get(&pid) {
                let dead_update = GossipUpdate::dead(pid, member.incarnation);
                let msg = serde_json::json!({ "type": "gossip", "updates": [dead_update] });
                self.server.broadcast(msg.to_string().as_bytes()).await;
                info!("Broadcast dead gossip for {}", peer_id);
            }
        }
    }

    /// Try to parse a message as JSON.
    ///
    /// Returns parsed message type, or None if not JSON.
    fn try_parse_json_message(data: &[u8]) -> Option<JsonMessage> {
        let text = std::str::from_utf8(data).ok()?;
        let msg: Value = serde_json::from_str(text).ok()?;

        let msg_type = msg.get("type")?.as_str()?;

        match msg_type {
            "gossip" => {
                let updates = msg.get("updates").and_then(|u| u.as_array())?;
                let parsed: Vec<GossipUpdate> = updates
                    .iter()
                    .filter_map(|v| serde_json::from_value(v.clone()).ok())
                    .collect();
                Some(JsonMessage::Gossip(parsed))
            }
            "sync" => {
                // Extract piggybacked gossip
                let gossip: Vec<GossipUpdate> = msg
                    .get("gossip")
                    .and_then(|g| g.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|v| serde_json::from_value(v.clone()).ok())
                            .collect()
                    })
                    .unwrap_or_default();

                // Extract sync data
                let data = msg.get("data").and_then(|d| d.as_array()).map(|arr| {
                    arr.iter()
                        .filter_map(|v| v.as_u64().map(|n| n as u8))
                        .collect()
                })?;

                Some(JsonMessage::Sync { data, gossip })
            }
            _ => None,
        }
    }

    /// Handle gossip updates from a peer and relay to others.
    async fn handle_gossip_updates(
        &mut self,
        updates: &[GossipUpdate],
        from_peer_id: &str,
        from_temp_id: &str,
    ) {
        if updates.is_empty() {
            return;
        }

        if let Ok(from_pid) = from_peer_id.parse::<PeerId>() {
            let new_peers = self.membership.process_gossip(updates, from_pid);
            debug!(
                "Processed {} gossip updates from {}, discovered {} new peers",
                updates.len(),
                from_peer_id,
                new_peers.len()
            );

            // Relay gossip to other peers (exclude sender)
            if self.server.peer_count() > 1 {
                let relay_msg = serde_json::json!({ "type": "gossip", "updates": updates });
                self.server
                    .broadcast_except(relay_msg.to_string().as_bytes(), from_temp_id)
                    .await;
                debug!(
                    "Relayed {} gossip updates to {} other peer(s)",
                    updates.len(),
                    self.server.peer_count() - 1
                );
            }

            // TODO: Auto-connect to newly discovered server peers
            for peer in new_peers {
                if let Some(addr) = &peer.address {
                    info!(
                        "Discovered peer {} at {} (auto-connect TODO)",
                        peer.peer_id, addr
                    );
                }
            }
        }
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Set up logging - respects RUST_LOG env var, defaults to info (or debug with --verbose)
    let default_filter = if args.verbose {
        "debug,sync_daemon=debug"
    } else {
        "info,sync_daemon=info"
    };
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter));
    tracing_subscriber::fmt().with_env_filter(filter).init();

    // Handle subcommands
    if let Some(Command::AddPeer { address }) = args.command {
        info!("add-peer command: {}", address);
        // TODO: Connect to running daemon via IPC and add peer
        eprintln!("add-peer subcommand not yet implemented");
        eprintln!("For now, use --bootstrap {} on daemon startup", address);
        return Ok(());
    }

    info!("Starting sync-daemon");
    info!("Vault path: {:?}", args.vault);
    if !args.client_only {
        info!("Listen address: {}", args.listen);
    }
    if let Some(ref advertise) = args.advertise {
        info!("Advertised address: {}", advertise);
    }
    if args.client_only {
        info!("Running in client-only mode (no incoming connections)");
    }

    // Generate or parse peer ID
    let peer_id: sync_core::PeerId = match args.peer_id {
        Some(id_str) => id_str.parse().context("Invalid peer ID")?,
        None => {
            let id = sync_core::PeerId::generate();
            info!("Generated peer ID: {}", id);
            id
        }
    };

    // Create filesystem
    let fs = NativeFs::new(args.vault.clone());

    // Initialize or load vault
    let vault = if fs.exists(".sync").await? {
        info!("Loading existing vault");
        Vault::load(fs, peer_id).await?
    } else {
        info!("Initializing new vault");
        Vault::init(fs, peer_id).await?
    };

    info!("Vault loaded, peer ID: {}", vault.peer_id());

    // Create WebSocket server (takes string peer_id for protocol messages)
    let (server, mut peer_connected_rx) = WebSocketServer::new(peer_id.to_string(), args.advertise.clone());

    // Create connection manager for outgoing connections
    let (outgoing, mut outgoing_rx) = ConnectionManager::new(
        peer_id.to_string(),
        args.advertise.clone(),
    );

    // Only listen for incoming connections if not in client-only mode
    let listener = if !args.client_only {
        Some(WebSocketServer::bind(&args.listen).await?)
    } else {
        None
    };

    // Create file watcher
    let watcher = FileWatcher::new(args.vault.clone())?;
    info!("File watcher started");

    // Create SWIM membership list for gossip-based peer discovery
    let membership = MembershipList::new(peer_id, args.advertise.clone());

    // Create daemon state
    let mut daemon = Daemon {
        vault: Arc::new(Mutex::new(vault)),
        server,
        outgoing,
        watcher,
        membership,
    };

    // Connect to bootstrap peers
    for bootstrap_addr in &args.bootstrap {
        info!("Connecting to bootstrap peer: {}", bootstrap_addr);
        if let Err(e) = daemon.outgoing.connect_to(bootstrap_addr).await {
            error!("Failed to connect to bootstrap peer {}: {}", bootstrap_addr, e);
        }
    }

    info!("Daemon running. Press Ctrl+C to stop.");

    // Main event loop
    loop {
        // Create accept future only if we have a listener
        let accept_future = async {
            if let Some(ref l) = listener {
                Some(l.accept().await)
            } else {
                std::future::pending::<Option<std::io::Result<_>>>().await
            }
        };

        tokio::select! {
            // Accept new WebSocket connections (if listening)
            Some(result) = accept_future => {
                match result {
                    Ok((stream, addr)) => {
                        daemon.server.accept_connection(stream, addr).await;
                    }
                    Err(e) => {
                        error!("Failed to accept connection: {}", e);
                    }
                }
            }

            // Handle file watcher events
            Some(event) = daemon.watcher.event_rx().recv() => {
                daemon.on_file_changed(event).await;
            }

            // Handle WebSocket events (messages, handshakes, closes)
            Some(event) = daemon.server.recv_event() => {
                match event {
                    ConnectionEvent::Message(msg) => {
                        daemon.on_sync_message(msg).await;
                    }
                    ConnectionEvent::Handshake { temp_id, peer_id, address } => {
                        debug!("Handshake event for {} -> {} (address: {:?})", temp_id, peer_id, address);
                        daemon.server.register_peer(&temp_id, peer_id, address);
                    }
                    ConnectionEvent::Closed { temp_id } => {
                        // Get real peer_id before removing (for SWIM tracking)
                        if let Some(peer_id) = daemon.server.resolve_peer_id(&temp_id) {
                            info!("Peer disconnected: {} (was {})", peer_id, temp_id);
                            daemon.on_peer_disconnected(&peer_id);
                            daemon.broadcast_dead_gossip(&peer_id).await;
                        } else {
                            info!("Connection closed before handshake: {}", temp_id);
                        }
                        daemon.server.remove_peer(&temp_id);
                    }
                }
            }

            // Handle peer connected notifications (for sync init)
            Some((peer_id, address)) = peer_connected_rx.recv() => {
                daemon.on_peer_connected(peer_id, address).await;
            }

            // Handle outgoing connection events
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
                        daemon.broadcast_dead_gossip(&peer_id).await;
                    }
                    ManagerEvent::PeerDiscovered { peer_id, address } => {
                        info!("Discovered peer {} at {}", peer_id, address);
                        // TODO: Auto-connect to discovered peers
                    }
                }
            }

            // Handle graceful shutdown
            _ = tokio::signal::ctrl_c() => {
                info!("Shutdown signal received");
                break;
            }
        }
    }

    info!("Shutting down");
    Ok(())
}
