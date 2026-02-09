//! WASM bindings for sync-core.
//!
//! Provides the bridge between TypeScript (Obsidian plugin) and Rust (sync-core).
//!
//! # Architecture
//!
//! The TypeScript plugin creates a `JsFileSystemBridge` with callbacks that access
//! Obsidian's Vault API. This bridge implements the `FileSystem` trait, allowing
//! the Rust `Vault` to read/write files through JavaScript.
//!
//! ```text
//! TypeScript                    WASM (Rust)
//! ──────────                    ───────────
//! ObsidianFs ──callbacks──> JsFileSystemBridge
//!                                   │
//!                                   ▼
//!                           impl FileSystem
//!                                   │
//!                                   ▼
//!                           Vault<JsFileSystemBridge>
//!                                   │
//!                                   ▼
//!                              WasmVault (exposed to JS)
//! ```
//!
//! **Note**: This crate only compiles for `wasm32` targets. When building for native
//! targets (e.g., during `cargo check --workspace`), this crate provides no exports.

#[cfg(target_arch = "wasm32")]
mod fs_bridge;

#[cfg(target_arch = "wasm32")]
pub use fs_bridge::JsFileSystemBridge;

// ============================================================================
// All WASM-specific code is gated behind target_arch = "wasm32"
// This allows `cargo check --workspace` to succeed on native targets.
// ============================================================================

#[cfg(target_arch = "wasm32")]
mod wasm_impl {
    use super::*;
    use serde::{Deserialize, Serialize};
    use std::cell::RefCell;
    use tracing_subscriber::layer::SubscriberExt;
    use wasm_bindgen::prelude::*;

    // ========== Callback Logger Layer ==========

    /// Store the logger callback in thread-local storage (WASM is single-threaded)
    thread_local! {
        static LOGGER_CALLBACK: RefCell<Option<js_sys::Function>> = const { RefCell::new(None) };
    }

    /// A tracing layer that invokes a JavaScript callback for each log event.
    struct JsCallbackLayer;

    impl<S> tracing_subscriber::Layer<S> for JsCallbackLayer
    where
        S: tracing::Subscriber,
    {
        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            LOGGER_CALLBACK.with(|cb| {
                if let Some(callback) = cb.borrow().as_ref() {
                    // Extract event data
                    let metadata = event.metadata();
                    let level = metadata.level().as_str();
                    let target = metadata.target();

                    // Build message from event fields
                    let mut visitor = MessageVisitor::default();
                    event.record(&mut visitor);
                    let message = visitor.message;

                    // Get timestamp in milliseconds
                    let timestamp = web_time::SystemTime::now()
                        .duration_since(web_time::UNIX_EPOCH)
                        .map(|d| d.as_millis() as f64)
                        .unwrap_or(0.0);

                    // Create JS object for the event
                    let js_event = js_sys::Object::new();
                    let _ = js_sys::Reflect::set(&js_event, &"level".into(), &level.into());
                    let _ = js_sys::Reflect::set(&js_event, &"target".into(), &target.into());
                    let _ = js_sys::Reflect::set(&js_event, &"message".into(), &message.into());
                    let _ = js_sys::Reflect::set(&js_event, &"timestamp".into(), &timestamp.into());

                    // Call the JavaScript callback
                    let _ = callback.call1(&JsValue::NULL, &js_event);
                }
            });
        }
    }

    /// Visitor to extract message from tracing event fields
    #[derive(Default)]
    struct MessageVisitor {
        message: String,
    }

    impl tracing::field::Visit for MessageVisitor {
        fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
            if field.name() == "message" {
                self.message = format!("{:?}", value);
            } else if self.message.is_empty() {
                // Build message from all fields if no explicit message
                if !self.message.is_empty() {
                    self.message.push_str(", ");
                }
                self.message.push_str(&format!("{}={:?}", field.name(), value));
            } else {
                // Append additional fields
                self.message.push_str(&format!(" {}={:?}", field.name(), value));
            }
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            if field.name() == "message" {
                self.message = value.to_string();
            } else if self.message.is_empty() {
                self.message = format!("{}={}", field.name(), value);
            } else {
                self.message.push_str(&format!(" {}={}", field.name(), value));
            }
        }
    }

    /// Configuration for WASM initialization
    #[derive(Default, Deserialize)]
    #[serde(default)]
    struct InitConfig {
        /// Whether a logger callback was provided (the actual function is passed separately)
        #[serde(skip)]
        has_logger: bool,
    }

    /// Initialize the WASM module (sets up panic hook and tracing for better debugging).
    ///
    /// Accepts an optional configuration object:
    /// - `init()` - console-only logging (default)
    /// - `init({})` - console-only logging
    /// - `init({ logger: (event) => {...} })` - callback + console logging
    ///
    /// The logger callback receives events with: `{ level, target, message, timestamp }`
    #[wasm_bindgen]
    pub fn init(config: Option<js_sys::Object>) {
        console_error_panic_hook::set_once();

        // Check if config has a logger callback
        let has_callback = config.as_ref().map_or(false, |cfg| {
            js_sys::Reflect::get(cfg, &"logger".into())
                .ok()
                .map_or(false, |v| v.is_function())
        });

        if has_callback {
            // Extract and store the logger callback
            let callback = config
                .as_ref()
                .and_then(|cfg| js_sys::Reflect::get(cfg, &"logger".into()).ok())
                .and_then(|v| v.dyn_into::<js_sys::Function>().ok());

            if let Some(cb) = callback {
                LOGGER_CALLBACK.with(|cell| {
                    *cell.borrow_mut() = Some(cb);
                });
            }

            // Use combined subscriber: callback layer + console layer
            let console_layer = tracing_wasm::WASMLayer::new(
                tracing_wasm::WASMLayerConfigBuilder::new()
                    .set_max_level(tracing::Level::DEBUG)
                    .build(),
            );

            let subscriber = tracing_subscriber::registry()
                .with(JsCallbackLayer)
                .with(console_layer);

            tracing::subscriber::set_global_default(subscriber).ok();
        } else {
            // Default: console-only logging
            tracing_wasm::set_as_global_default_with_config(
                tracing_wasm::WASMLayerConfigBuilder::new()
                    .set_max_level(tracing::Level::DEBUG)
                    .build(),
            );
        }

        log("sync-wasm initialized");
    }

    /// Health check to verify WASM is working
    #[wasm_bindgen]
    pub fn health_check() -> u32 {
        42
    }

    /// Get version string
    #[wasm_bindgen]
    pub fn version() -> String {
        env!("CARGO_PKG_VERSION").to_string()
    }

    /// Generate a new random peer ID.
    ///
    /// Returns a 16-character hex string that uniquely identifies this peer.
    /// Store this in settings and pass to `init()` or `load()`.
    #[wasm_bindgen(js_name = generatePeerId)]
    pub fn generate_peer_id() -> String {
        sync_core::PeerId::generate().to_string()
    }

    #[wasm_bindgen]
    extern "C" {
        #[wasm_bindgen(js_namespace = console)]
        pub fn log(s: &str);

        #[wasm_bindgen(js_namespace = console, js_name = log)]
        pub fn log_val(v: &JsValue);

        #[wasm_bindgen(js_namespace = console)]
        pub fn error(s: &str);
    }

    // ========== WASM Subscription Handle ==========

    /// Subscription handle exposed to JavaScript.
    ///
    /// Call `dispose()` to unsubscribe, or let the JS garbage collector
    /// collect it (the Rust Drop will run via FinalizationRegistry).
    #[wasm_bindgen]
    pub struct WasmSubscription {
        inner: RefCell<Option<sync_core::Subscription>>,
    }

    #[wasm_bindgen]
    impl WasmSubscription {
        /// Unsubscribe from events. Safe to call multiple times.
        pub fn dispose(&self) {
            self.inner.borrow_mut().take(); // Drop the inner Subscription
        }
    }

    /// Vault manager exposed to TypeScript.
    ///
    /// Wraps the core `Vault` and provides async methods that work with JS Promises.
    #[wasm_bindgen]
    pub struct WasmVault {
        inner: sync_core::Vault<fs_bridge::JsFileSystemBridge>,
    }

    #[wasm_bindgen]
    impl WasmVault {
        /// Initialize a new vault (creates .sync directory).
        ///
        /// Call this when the user clicks "Initialize Sync" for the first time.
        /// The `peer_id` should be a hex string from `generatePeerId()` or a legacy UUID.
        #[wasm_bindgen]
        pub async fn init(fs: fs_bridge::JsFileSystemBridge, peer_id: String) -> Result<WasmVault, JsError> {
            let peer_id: sync_core::PeerId = peer_id
                .parse()
                .map_err(|e| JsError::new(&format!("Invalid peer ID: {}", e)))?;

            let inner = sync_core::Vault::init(fs, peer_id)
                .await
                .map_err(|e| JsError::new(&e.to_string()))?;

            Ok(WasmVault { inner })
        }

        /// Load an existing vault and reconcile with filesystem.
        ///
        /// Call this on plugin startup if vault is already initialized.
        /// Reconciliation detects files added/modified/deleted while plugin was off.
        /// The `peer_id` should be a hex string from `generatePeerId()` or a legacy UUID.
        ///
        /// Returns a report of what was reconciled.
        #[wasm_bindgen]
        pub async fn load(fs: fs_bridge::JsFileSystemBridge, peer_id: String) -> Result<WasmVault, JsError> {
            let peer_id: sync_core::PeerId = peer_id
                .parse()
                .map_err(|e| JsError::new(&format!("Invalid peer ID: {}", e)))?;

            let inner = sync_core::Vault::load(fs, peer_id)
                .await
                .map_err(|e| JsError::new(&e.to_string()))?;

            Ok(WasmVault { inner })
        }
        
        /// Manually trigger reconciliation.
        /// 
        /// This is automatically called during `load()`, but can be called again
        /// if needed (e.g., after detecting external filesystem changes).
        #[wasm_bindgen]
        pub async fn reconcile(&self) -> Result<JsValue, JsError> {
            let report = self.inner
                .reconcile()
                .await
                .map_err(|e| JsError::new(&e.to_string()))?;
            
            let js_report = ReconcileReportJs {
                indexed: report.indexed,
                reindexed: report.reindexed,
                orphaned: report.orphaned,
            };
            
            serde_wasm_bindgen::to_value(&js_report).map_err(|e| JsError::new(&e.to_string()))
        }

        /// Get our peer ID.
        #[wasm_bindgen(js_name = peerId)]
        pub fn peer_id(&self) -> String {
            self.inner.peer_id().to_string()
        }

        /// Check if vault is initialized (has .sync directory).
        #[wasm_bindgen(js_name = isInitialized)]
        pub async fn is_initialized(&self) -> Result<bool, JsError> {
            self.inner
                .is_initialized()
                .await
                .map_err(|e| JsError::new(&e.to_string()))
        }

        /// Handle a file change event from Obsidian.
        ///
        /// Call this when Obsidian fires modify/create events for markdown files.
        /// Updates the Loro document to match the file content.
        #[wasm_bindgen(js_name = onFileChanged)]
        pub async fn on_file_changed(&self, path: &str) -> Result<(), JsError> {
            self.inner
                .on_file_changed(path)
                .await
                .map_err(|e| JsError::new(&e.to_string()))
        }

        /// Get the version vector for a document as encoded bytes.
        ///
        /// Returns null if the document hasn't been loaded/doesn't exist.
        /// Use this to track the synced version and detect if subsequent
        /// modifications are purely from sync or include local edits.
        #[wasm_bindgen(js_name = getDocumentVersion)]
        pub async fn get_document_version(&self, path: &str) -> Result<JsValue, JsError> {
            let version = self
                .inner
                .get_document_version(path)
                .await
                .map_err(|e| JsError::new(&e.to_string()))?;

            match version {
                Some(bytes) => {
                    let array = js_sys::Uint8Array::from(bytes.as_slice());
                    Ok(array.into())
                }
                None => Ok(JsValue::NULL),
            }
        }

        /// Check if a document's current version includes all operations from a synced version.
        ///
        /// Returns true if `current_version` contains all operations from `synced_version`.
        /// Use this to detect if a file modification event is purely from sync
        /// (should be skipped to prevent re-broadcast) or includes local edits.
        #[wasm_bindgen(js_name = versionIncludes)]
        pub fn version_includes(current_version: &[u8], synced_version: &[u8]) -> bool {
            sync_core::Vault::<sync_core::fs::InMemoryFs>::version_includes(current_version, synced_version)
        }

        /// List all markdown files in the vault.
        #[wasm_bindgen(js_name = listFiles)]
        pub async fn list_files(&self) -> Result<JsValue, JsError> {
            let files = self
                .inner
                .list_files()
                .await
                .map_err(|e| JsError::new(&e.to_string()))?;

            serde_wasm_bindgen::to_value(&files).map_err(|e| JsError::new(&e.to_string()))
        }

        // ========== Sync Protocol Methods ==========

        /// Prepare a sync request to send to a newly connected peer.
        ///
        /// Returns serialized bytes containing our version vectors for all documents.
        /// Send this to the peer immediately after connection.
        #[wasm_bindgen(js_name = prepareSyncRequest)]
        pub async fn prepare_sync_request(&self) -> Result<Vec<u8>, JsError> {
            self.inner
                .prepare_sync_request()
                .await
                .map_err(|e| JsError::new(&e.to_string()))
        }

        /// Process an incoming sync message from a peer.
        ///
        /// Returns a tuple of:
        /// - Optional response bytes to send back to the peer
        /// - Array of file paths that were modified (need to be saved/reloaded)
        ///
        /// Call this when you receive a message from a peer.
        #[wasm_bindgen(js_name = processSyncMessage)]
        pub async fn process_sync_message(&self, data: &[u8]) -> Result<JsValue, JsError> {
            log(&format!("processSyncMessage: received {} bytes", data.len()));

            let (response, modified_paths) = self
                .inner
                .process_sync_message(data)
                .await
                .map_err(|e| {
                    error(&format!("processSyncMessage error: {}", e));
                    JsError::new(&e.to_string())
                })?;

            log(&format!("processSyncMessage: response={}, modified={:?}",
                response.as_ref().map(|r| r.len()).unwrap_or(0),
                modified_paths));

            // Return as a JS object: { response: Uint8Array | null, modifiedPaths: string[] }
            let result = SyncMessageResult {
                response,
                modified_paths,
            };

            serde_wasm_bindgen::to_value(&result)
                .map_err(|e| JsError::new(&e.to_string()))
        }

        /// Prepare a document update to broadcast after a local file change.
        ///
        /// Returns serialized bytes to send to all connected peers,
        /// or null if no update is needed.
        ///
        /// Call this after `onFileChanged` to get the update to broadcast.
        #[wasm_bindgen(js_name = prepareDocumentUpdate)]
        pub async fn prepare_document_update(&self, path: &str) -> Result<JsValue, JsError> {
            let update = self
                .inner
                .prepare_document_update(path)
                .await
                .map_err(|e| JsError::new(&e.to_string()))?;

            match update {
                Some(bytes) => {
                    let array = js_sys::Uint8Array::from(bytes.as_slice());
                    Ok(array.into())
                }
                None => Ok(JsValue::NULL),
            }
        }

        // ========== File Tree Operations ==========

        /// Delete a file from the tree (CRDT operation).
        ///
        /// Call this when Obsidian fires a delete event for a markdown file.
        /// The deletion is tracked in the registry LoroTree and syncs to peers.
        #[wasm_bindgen(js_name = deleteFile)]
        pub async fn delete_file(&self, path: &str) -> Result<(), JsError> {
            self.inner
                .delete_file(path)
                .await
                .map_err(|e| JsError::new(&e.to_string()))
        }

        /// Rename/move a file in the tree (CRDT operation).
        ///
        /// Call this when Obsidian fires a rename event for a markdown file.
        /// The rename is tracked in the registry LoroTree and syncs to peers.
        #[wasm_bindgen(js_name = renameFile)]
        pub async fn rename_file(&self, old_path: &str, new_path: &str) -> Result<(), JsError> {
            self.inner
                .rename_file(old_path, new_path)
                .await
                .map_err(|e| JsError::new(&e.to_string()))
        }

        /// Check if a file is deleted in the tree.
        ///
        /// Returns true if the file is deleted or not found in the tree.
        #[wasm_bindgen(js_name = isFileDeleted)]
        pub fn is_file_deleted(&self, path: &str) -> bool {
            self.inner.is_file_deleted(path)
        }

        /// Check if a file was just synced (and consume the flag).
        ///
        /// Returns true once if the file was synced, false on subsequent calls.
        /// Use this in file watcher handlers to skip re-broadcasting files we just received.
        #[wasm_bindgen(js_name = consumeSyncFlag)]
        pub fn consume_sync_flag(&self, path: &str) -> bool {
            self.inner.consume_sync_flag(path)
        }

        /// Prepare a file deletion message to broadcast to peers.
        ///
        /// Call this after `deleteFile` to get the message to broadcast.
        #[wasm_bindgen(js_name = prepareFileDeleted)]
        pub fn prepare_file_deleted(&self, path: &str) -> Result<JsValue, JsError> {
            let bytes = self
                .inner
                .prepare_file_deleted(path)
                .map_err(|e| JsError::new(&e.to_string()))?;

            let array = js_sys::Uint8Array::from(bytes.as_slice());
            Ok(array.into())
        }

        /// Prepare a file renamed message to broadcast to peers.
        ///
        /// Call this after `renameFile` to get the message to broadcast.
        #[wasm_bindgen(js_name = prepareFileRenamed)]
        pub fn prepare_file_renamed(&self, old_path: &str, new_path: &str) -> Result<JsValue, JsError> {
            let bytes = self
                .inner
                .prepare_file_renamed(old_path, new_path)
                .map_err(|e| JsError::new(&e.to_string()))?;

            let array = js_sys::Uint8Array::from(bytes.as_slice());
            Ok(array.into())
        }

        // ========== Debug API Methods ==========

        /// Get the registry version vector.
        ///
        /// Returns an object mapping peer ID hex strings to counter values.
        #[wasm_bindgen(js_name = getRegistryVersion)]
        pub fn get_registry_version(&self) -> Result<JsValue, JsError> {
            let version = self.inner.get_registry_version();
            // Use serialize_maps_as_objects to return a plain JS object instead of Map
            let serializer = serde_wasm_bindgen::Serializer::new().serialize_maps_as_objects(true);
            version.serialize(&serializer)
                .map_err(|e| JsError::new(&e.to_string()))
        }

        /// Get registry oplog statistics.
        ///
        /// Returns `{ changeCount, opCount }`.
        #[wasm_bindgen(js_name = getRegistryStats)]
        pub fn get_registry_stats(&self) -> Result<JsValue, JsError> {
            let stats = self.inner.get_registry_stats();
            serde_wasm_bindgen::to_value(&stats)
                .map_err(|e| JsError::new(&e.to_string()))
        }

        /// Get cheap metadata from the .loro blob header.
        ///
        /// Returns blob metadata (version vectors, timestamps, change count) without
        /// loading the full document. Returns `null` if the document doesn't exist.
        #[wasm_bindgen(js_name = getDocumentBlobMeta)]
        pub async fn get_document_blob_meta(&self, path: &str) -> Result<JsValue, JsError> {
            let meta = self.inner.get_document_blob_meta(path).await
                .map_err(|e| JsError::new(&e.to_string()))?;
            match meta {
                Some(m) => serde_wasm_bindgen::to_value(&m)
                    .map_err(|e| JsError::new(&e.to_string())),
                None => Ok(JsValue::NULL),
            }
        }

        /// Get full document info (requires loading the document).
        ///
        /// Returns content metadata including body length, frontmatter status, and doc_id.
        /// Returns `null` if the document doesn't exist.
        #[wasm_bindgen(js_name = getDocumentInfo)]
        pub async fn get_document_info(&self, path: &str) -> Result<JsValue, JsError> {
            let info = self.inner.get_document_info(path).await
                .map_err(|e| JsError::new(&e.to_string()))?;
            match info {
                Some(i) => serde_wasm_bindgen::to_value(&i)
                    .map_err(|e| JsError::new(&e.to_string())),
                None => Ok(JsValue::NULL),
            }
        }

        // ========== Peer Management Methods ==========

        /// Notify that a peer has connected (call after handshake completes).
        ///
        /// Updates the registry and emits a `PeerConnected` event.
        /// Returns the `ConnectedPeer` info.
        #[wasm_bindgen(js_name = peerConnected)]
        pub fn peer_connected(
            &self,
            peer_id: String,
            address: String,
            direction: String,
        ) -> Result<JsValue, JsError> {
            let dir = match direction.as_str() {
                "incoming" => sync_core::peers::ConnectionDirection::Incoming,
                "outgoing" => sync_core::peers::ConnectionDirection::Outgoing,
                _ => return Err(JsError::new("direction must be 'incoming' or 'outgoing'")),
            };

            let peer = self.inner.peer_connected(peer_id, address, dir)
                .map_err(|e| JsError::new(&e.to_string()))?;
            serde_wasm_bindgen::to_value(&peer)
                .map_err(|e| JsError::new(&e.to_string()))
        }

        /// Notify that a peer has disconnected.
        ///
        /// Updates the registry and emits a `PeerDisconnected` event if known.
        #[wasm_bindgen(js_name = peerDisconnected)]
        pub fn peer_disconnected(&self, peer_id: String, reason: String) -> Result<(), JsError> {
            let reason = match reason.as_str() {
                "userRequested" => sync_core::peers::DisconnectReason::UserRequested,
                "networkError" => sync_core::peers::DisconnectReason::NetworkError,
                "remoteClosed" => sync_core::peers::DisconnectReason::RemoteClosed,
                "protocolError" => sync_core::peers::DisconnectReason::ProtocolError,
                _ => return Err(JsError::new("reason must be 'userRequested', 'networkError', 'remoteClosed', or 'protocolError'")),
            };
            self.inner.peer_disconnected(&peer_id, reason);
            Ok(())
        }

        /// Called when WebSocket opens (before handshake).
        /// Creates peer in Connecting state, indexed by connection ID.
        #[wasm_bindgen(js_name = peerConnecting)]
        pub fn peer_connecting(
            &self,
            connection_id: String,
            address: String,
            direction: String,
        ) -> Result<JsValue, JsError> {
            let dir = match direction.as_str() {
                "incoming" => sync_core::peers::ConnectionDirection::Incoming,
                "outgoing" => sync_core::peers::ConnectionDirection::Outgoing,
                _ => return Err(JsError::new("direction must be 'incoming' or 'outgoing'")),
            };

            let peer = self.inner.peer_connecting(connection_id, address, dir);
            serde_wasm_bindgen::to_value(&peer)
                .map_err(|e| JsError::new(&e.to_string()))
        }

        /// Called when handshake completes. Maps connection_id to real peer_id.
        /// Returns error if connection_id unknown.
        #[wasm_bindgen(js_name = peerHandshakeComplete)]
        pub fn peer_handshake_complete(
            &self,
            connection_id: String,
            peer_id: String,
        ) -> Result<JsValue, JsError> {
            let peer = self.inner.peer_handshake_complete(&connection_id, peer_id)
                .map_err(|e| JsError::new(&e.to_string()))?;
            serde_wasm_bindgen::to_value(&peer)
                .map_err(|e| JsError::new(&e.to_string()))
        }

        /// Get peer by connection ID (for pre-handshake lookups).
        /// Returns null if not found.
        #[wasm_bindgen(js_name = getPeerByConnectionId)]
        pub fn get_peer_by_connection_id(&self, connection_id: String) -> Result<JsValue, JsError> {
            match self.inner.get_peer_by_connection_id(&connection_id) {
                Some(peer) => serde_wasm_bindgen::to_value(&peer)
                    .map_err(|e| JsError::new(&e.to_string())),
                None => Ok(JsValue::NULL),
            }
        }

        /// Resolve connection ID to peer ID (returns connection_id if no mapping).
        #[wasm_bindgen(js_name = resolvePeerId)]
        pub fn resolve_peer_id(&self, connection_id: String) -> String {
            self.inner.resolve_peer_id(&connection_id)
        }

        /// Get all peers seen this session (connected and disconnected).
        #[wasm_bindgen(js_name = getKnownPeers)]
        pub fn get_known_peers(&self) -> Result<JsValue, JsError> {
            let peers = self.inner.get_known_peers();
            serde_wasm_bindgen::to_value(&peers)
                .map_err(|e| JsError::new(&e.to_string()))
        }

        /// Get info for a specific peer.
        ///
        /// Returns `null` if the peer is not known.
        #[wasm_bindgen(js_name = getPeerInfo)]
        pub fn get_peer_info(&self, peer_id: String) -> Result<JsValue, JsError> {
            match self.inner.get_peer_info(&peer_id) {
                Some(peer) => serde_wasm_bindgen::to_value(&peer)
                    .map_err(|e| JsError::new(&e.to_string())),
                None => Ok(JsValue::NULL),
            }
        }

        /// Get currently connected peers only.
        #[wasm_bindgen(js_name = getConnectedPeers)]
        pub fn get_connected_peers(&self) -> Result<JsValue, JsError> {
            let peers = self.inner.get_connected_peers();
            serde_wasm_bindgen::to_value(&peers)
                .map_err(|e| JsError::new(&e.to_string()))
        }

        // ========== Sync Event Subscriptions ==========

        /// Subscribe to sync events for real-time monitoring.
        ///
        /// Returns a `WasmSubscription` handle. Call `dispose()` on it to unsubscribe,
        /// or let the JS garbage collector clean it up.
        #[wasm_bindgen(js_name = subscribeSyncEvents)]
        pub fn subscribe_sync_events(&self, callback: js_sys::Function) -> WasmSubscription {
            let rust_closure = move |event: sync_core::SyncEvent| {
                if let Ok(js_event) = serde_wasm_bindgen::to_value(&event) {
                    let _ = callback.call1(&wasm_bindgen::JsValue::NULL, &js_event);
                }
            };

            WasmSubscription {
                inner: RefCell::new(Some(self.inner.subscribe(rust_closure))),
            }
        }
    }

    /// Result from processing a sync message
    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    struct SyncMessageResult {
        /// Response to send back (if any)
        #[serde(with = "optional_bytes")]
        response: Option<Vec<u8>>,
        /// Paths of files that were modified
        modified_paths: Vec<String>,
    }

    /// Report from reconciliation for JS
    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    struct ReconcileReportJs {
        /// Files that were newly indexed
        indexed: Vec<String>,
        /// Files that were re-indexed (modified externally)
        reindexed: Vec<String>,
        /// Orphaned .loro file hashes
        orphaned: Vec<String>,
    }

    /// Serialize Option<Vec<u8>> as null or Uint8Array-compatible array
    mod optional_bytes {
        use serde::Serializer;

        pub fn serialize<S>(value: &Option<Vec<u8>>, serializer: S) -> Result<S::Ok, S::Error>
        where
            S: Serializer,
        {
            match value {
                Some(bytes) => serializer.serialize_bytes(bytes),
                None => serializer.serialize_none(),
            }
        }
    }

    // ========== SWIM Gossip Protocol ==========

    /// WASM wrapper for SWIM membership list.
    ///
    /// Tracks known peers and their states (Alive, Suspected, Dead, Removed).
    /// Used for gossip-based peer discovery.
    #[wasm_bindgen]
    pub struct WasmMembership {
        inner: RefCell<sync_core::swim::MembershipList>,
    }

    #[wasm_bindgen]
    impl WasmMembership {
        /// Create a new membership list for this peer.
        ///
        /// @param peerId - Our peer ID (hex string)
        /// @param address - Our address for incoming connections (null for client-only)
        /// @param incarnation - Our incarnation number (use saved value or 1 for new peers)
        #[wasm_bindgen(constructor)]
        pub fn new(
            peer_id: String,
            address: Option<String>,
            incarnation: u64,
        ) -> Result<WasmMembership, JsError> {
            let pid = peer_id.parse()
                .map_err(|e: sync_core::peer_id::PeerIdError| JsError::new(&e.to_string()))?;
            Ok(WasmMembership {
                inner: RefCell::new(sync_core::swim::MembershipList::with_incarnation(
                    pid, address, incarnation,
                )),
            })
        }

        /// Get our local incarnation number for persistence.
        #[wasm_bindgen(js_name = localIncarnation)]
        pub fn local_incarnation(&self) -> u64 {
            self.inner.borrow().local_incarnation()
        }

        /// Update our local address after construction.
        ///
        /// Use this when the server port is only known after startup.
        #[wasm_bindgen(js_name = setLocalAddress)]
        pub fn set_local_address(&self, address: String) {
            self.inner.borrow_mut().set_local_address(address);
        }

        /// Process received gossip updates from a peer.
        ///
        /// @param gossipJson - JSON array of GossipUpdate objects
        /// @param fromPeerId - Peer ID who sent the gossip
        /// @returns JSON array of newly discovered PeerInfo objects
        #[wasm_bindgen(js_name = processGossip)]
        pub fn process_gossip(&self, gossip_json: String, from_peer_id: String) -> Result<JsValue, JsError> {
            let updates: Vec<sync_core::swim::GossipUpdate> = serde_json::from_str(&gossip_json)
                .map_err(|e| JsError::new(&format!("Invalid gossip JSON: {}", e)))?;
            let from_pid = from_peer_id.parse()
                .map_err(|e: sync_core::peer_id::PeerIdError| JsError::new(&e.to_string()))?;

            let new_peers = self.inner.borrow_mut().process_gossip(&updates, from_pid);

            serde_wasm_bindgen::to_value(&new_peers)
                .map_err(|e| JsError::new(&e.to_string()))
        }

        /// Generate full gossip for initial sync with a new peer.
        ///
        /// @returns JSON array of GossipUpdate objects (Alive for all known members)
        #[wasm_bindgen(js_name = generateFullGossip)]
        pub fn generate_full_gossip(&self) -> Result<String, JsError> {
            let gossip = self.inner.borrow().generate_full_gossip();
            serde_json::to_string(&gossip)
                .map_err(|e: serde_json::Error| JsError::new(&e.to_string()))
        }

        /// Drain pending gossip updates for piggybacking on messages.
        ///
        /// @returns JSON array of GossipUpdate objects
        #[wasm_bindgen(js_name = drainGossip)]
        pub fn drain_gossip(&self) -> Result<String, JsError> {
            let gossip = self.inner.borrow_mut().drain_gossip();
            serde_json::to_string(&gossip)
                .map_err(|e: serde_json::Error| JsError::new(&e.to_string()))
        }

        /// Get count of known members (excluding ourselves).
        #[wasm_bindgen(js_name = memberCount)]
        pub fn member_count(&self) -> usize {
            self.inner.borrow().len()
        }

        /// Get list of alive members.
        ///
        /// @returns JSON array of member objects with peer info and state
        #[wasm_bindgen(js_name = getAliveMembers)]
        pub fn get_alive_members(&self) -> Result<JsValue, JsError> {
            let members: Vec<_> = self.inner.borrow()
                .alive_members()
                .map(|m| MemberInfo {
                    peer_id: m.info.peer_id.to_string(),
                    address: m.info.address.clone(),
                    incarnation: m.incarnation,
                })
                .collect();

            serde_wasm_bindgen::to_value(&members)
                .map_err(|e| JsError::new(&e.to_string()))
        }

        /// Get list of server members (have addresses).
        ///
        /// @returns JSON array of member objects
        #[wasm_bindgen(js_name = getServerMembers)]
        pub fn get_server_members(&self) -> Result<JsValue, JsError> {
            let members: Vec<_> = self.inner.borrow()
                .server_members()
                .map(|m| MemberInfo {
                    peer_id: m.info.peer_id.to_string(),
                    address: m.info.address.clone(),
                    incarnation: m.incarnation,
                })
                .collect();

            serde_wasm_bindgen::to_value(&members)
                .map_err(|e| JsError::new(&e.to_string()))
        }

        /// Get a peer's current incarnation number (for reconnection incarnation bumps).
        #[wasm_bindgen(js_name = getMemberIncarnation)]
        pub fn get_member_incarnation(&self, peer_id: String) -> Option<u64> {
            let pid = peer_id.parse().ok()?;
            self.inner.borrow().get(&pid).map(|m| m.incarnation)
        }

        /// Check if a specific peer is known and alive.
        #[wasm_bindgen(js_name = isAlive)]
        pub fn is_alive(&self, peer_id: String) -> bool {
            let Ok(pid) = peer_id.parse() else { return false };
            self.inner.borrow()
                .get(&pid)
                .map(|m| m.state == sync_core::swim::MemberState::Alive)
                .unwrap_or(false)
        }

        /// Check if we have a peer in our membership list.
        #[wasm_bindgen(js_name = contains)]
        pub fn contains(&self, peer_id: String) -> bool {
            let Ok(pid) = peer_id.parse() else { return false };
            self.inner.borrow().contains(&pid)
        }

        /// Mark a peer as dead (for failure detection).
        #[wasm_bindgen(js_name = markDead)]
        pub fn mark_dead(&self, peer_id: String) -> bool {
            let Ok(pid) = peer_id.parse() else { return false };
            self.inner.borrow_mut().mark_dead(pid)
        }

        /// Mark a peer as removed (collective forgetting).
        #[wasm_bindgen(js_name = markRemoved)]
        pub fn mark_removed(&self, peer_id: String) -> bool {
            let Ok(pid) = peer_id.parse() else { return false };
            self.inner.borrow_mut().mark_removed(pid)
        }
    }

    /// Member info for JS serialization
    #[derive(Serialize)]
    #[serde(rename_all = "camelCase")]
    struct MemberInfo {
        peer_id: String,
        address: Option<String>,
        incarnation: u64,
    }
}

// Re-export wasm_impl contents at crate root for wasm32 targets
#[cfg(target_arch = "wasm32")]
pub use wasm_impl::*;
