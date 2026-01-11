//! sync-daemon: Headless P2P sync daemon for home server.
//!
//! Uses the same sync-core as the Obsidian plugin, but runs as a native binary
//! with native filesystem and networking.

use anyhow::Result;
use clap::Parser;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{debug, error, info, warn, Level};
use tracing_subscriber::FmtSubscriber;

// Use library exports
use sync_daemon::connection::ConnectionEvent;
use sync_daemon::native_fs::NativeFs;
use sync_daemon::server::WebSocketServer;
use sync_daemon::watcher::{FileEvent, FileEventKind, FileWatcher};
use sync_daemon::IncomingMessage;

use sync_core::fs::FileSystem;
use sync_core::Vault;

#[derive(Parser, Debug)]
#[command(name = "sync-daemon")]
#[command(about = "P2P vault sync daemon")]
struct Args {
    /// Path to the vault directory
    #[arg(short, long)]
    vault: PathBuf,

    /// Address to listen on for incoming connections
    #[arg(short, long, default_value = "0.0.0.0:8080")]
    listen: String,

    /// Peer ID (generated if not provided)
    #[arg(long)]
    peer_id: Option<String>,

    /// Enable verbose logging
    #[arg(long)]
    verbose: bool,
}

/// Daemon state holding all components.
struct Daemon {
    /// The sync vault (behind mutex for async access)
    vault: Arc<Mutex<Vault<NativeFs>>>,
    /// WebSocket server
    server: WebSocketServer,
    /// File watcher
    watcher: FileWatcher,
    /// Last synced versions for race condition prevention
    last_synced_versions: HashMap<String, Vec<u8>>,
}

impl Daemon {
    /// Handle a file change event from the watcher.
    async fn on_file_changed(&mut self, event: FileEvent) {
        match event.kind {
            FileEventKind::Modified => {
                self.on_file_modified(&event.path).await;
            }
            FileEventKind::Deleted => {
                debug!("File deleted: {} (deletion sync not yet implemented)", event.path);
                // TODO: Implement deletion sync
            }
        }
    }

    /// Handle a file modification.
    async fn on_file_modified(&mut self, path: &str) {
        // Skip broadcast if no peers connected
        if self.server.peer_count() == 0 {
            return;
        }

        let mut vault = self.vault.lock().await;

        // Check if this is an echo from a sync we just applied
        if let Some(synced_version) = self.last_synced_versions.get(path) {
            match vault.get_document_version(path).await {
                Ok(Some(current_version)) => {
                    if Vault::<NativeFs>::version_includes(&current_version, synced_version) {
                        // Version unchanged - this is a sync echo, skip broadcast
                        debug!("Skipping broadcast for {} (sync echo)", path);
                        self.last_synced_versions.remove(path);
                        return;
                    }
                }
                Ok(None) => {
                    // Document doesn't exist, proceed with broadcast
                }
                Err(e) => {
                    warn!("Failed to get version for {}: {}", path, e);
                }
            }
            self.last_synced_versions.remove(path);
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

        debug!("Processing sync message from {} ({} bytes)", peer_id, msg.data.len());

        let mut vault = self.vault.lock().await;

        match vault.process_sync_message(&msg.data).await {
            Ok((response, modified_paths)) => {
                // Track synced versions for modified files
                for path in &modified_paths {
                    if let Ok(Some(version)) = vault.get_document_version(path).await {
                        self.last_synced_versions.insert(path.clone(), version);
                    }
                }

                // Send response if any
                if let Some(response_data) = response {
                    drop(vault); // Release lock before network I/O
                    if let Err(e) = self.server.send_by_temp_id(&msg.temp_id, &response_data).await {
                        error!("Failed to send sync response to {}: {}", peer_id, e);
                    }
                }

                if !modified_paths.is_empty() {
                    info!("Synced {} file(s) from {}", modified_paths.len(), peer_id);
                }
            }
            Err(e) => {
                error!("Failed to process sync message from {}: {}", peer_id, e);
            }
        }
    }

    /// Handle a newly connected peer (after handshake).
    async fn on_peer_connected(&mut self, peer_id: String) {
        info!("Peer connected: {}", peer_id);

        let mut vault = self.vault.lock().await;

        // Prepare and send sync request (bidirectional init)
        match vault.prepare_sync_request().await {
            Ok(request) => {
                drop(vault); // Release lock before network I/O
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
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();

    // Set up logging
    let level = if args.verbose {
        Level::DEBUG
    } else {
        Level::INFO
    };
    let subscriber = FmtSubscriber::builder().with_max_level(level).finish();
    tracing::subscriber::set_global_default(subscriber)?;

    info!("Starting sync-daemon");
    info!("Vault path: {:?}", args.vault);
    info!("Listen address: {}", args.listen);

    // Generate peer ID if not provided
    let peer_id = args.peer_id.unwrap_or_else(|| {
        let id = uuid::Uuid::new_v4().to_string();
        info!("Generated peer ID: {}", id);
        id
    });

    // Create filesystem
    let fs = NativeFs::new(args.vault.clone());

    // Initialize or load vault
    let vault = if fs.exists(".sync").await? {
        info!("Loading existing vault");
        Vault::load(fs, peer_id.clone()).await?
    } else {
        info!("Initializing new vault");
        Vault::init(fs, peer_id.clone()).await?
    };

    info!("Vault loaded, peer ID: {}", vault.peer_id());

    // Create WebSocket server
    let (server, mut peer_connected_rx) = WebSocketServer::new(peer_id);
    let listener = WebSocketServer::bind(&args.listen).await?;

    // Create file watcher
    let watcher = FileWatcher::new(args.vault.clone())?;
    info!("File watcher started");

    // Create daemon state
    let mut daemon = Daemon {
        vault: Arc::new(Mutex::new(vault)),
        server,
        watcher,
        last_synced_versions: HashMap::new(),
    };

    info!("Daemon running. Press Ctrl+C to stop.");

    // Main event loop
    loop {
        tokio::select! {
            // Accept new WebSocket connections
            result = listener.accept() => {
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
                    ConnectionEvent::Handshake { temp_id, peer_id } => {
                        // Handshake is handled internally by server
                        daemon.server.poll_events().await;
                        debug!("Handshake event for {} -> {}", temp_id, peer_id);
                    }
                    ConnectionEvent::Closed { temp_id } => {
                        daemon.server.poll_events().await;
                        info!("Peer disconnected: {}", temp_id);
                    }
                }
            }

            // Handle peer connected notifications (for sync init)
            Some(peer_id) = peer_connected_rx.recv() => {
                daemon.on_peer_connected(peer_id).await;
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
