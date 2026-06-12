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
                self.message
                    .push_str(&format!("{}={:?}", field.name(), value));
            } else {
                // Append additional fields
                self.message
                    .push_str(&format!(" {}={:?}", field.name(), value));
            }
        }

        fn record_str(&mut self, field: &tracing::field::Field, value: &str) {
            if field.name() == "message" {
                self.message = value.to_string();
            } else if self.message.is_empty() {
                self.message = format!("{}={}", field.name(), value);
            } else {
                self.message
                    .push_str(&format!(" {}={}", field.name(), value));
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
    /// Returns a 64-character hex string that uniquely identifies this peer.
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
        /// Initialize a new vault (creates .sync directory and generates VaultId).
        ///
        /// Call this when the user clicks "Initialize Sync" for the first time.
        /// The VaultId is generated automatically and persisted in `.sync/metadata.toml`.
        ///
        /// `secret_key` is this device's 32-byte ed25519 secret key; the device's
        /// Loro author PeerId is derived from it so this replica authors operations
        /// under a device-unique id (see Loro Peer ID Semantics).
        #[wasm_bindgen]
        pub async fn init(
            fs: fs_bridge::JsFileSystemBridge,
            secret_key: &[u8],
        ) -> Result<WasmVault, JsError> {
            let key_bytes: [u8; 32] = secret_key
                .try_into()
                .map_err(|_| JsError::new("secret_key must be exactly 32 bytes"))?;
            let author = sync_core::PeerId::from_secret_bytes(key_bytes);

            let inner = sync_core::Vault::init(fs, author)
                .await
                .map_err(|e| JsError::new(&e.to_string()))?;

            Ok(WasmVault { inner })
        }

        /// Load an existing vault and reconcile with filesystem.
        ///
        /// Call this on plugin startup if vault is already initialized.
        /// Reconciliation detects files added/modified/deleted while plugin was off.
        /// The VaultId is read from `.sync/metadata.toml` (migrated from v0 if needed).
        ///
        /// `secret_key` is this device's 32-byte ed25519 secret key; the device's
        /// Loro author PeerId is derived from it (see `init`).
        #[wasm_bindgen]
        pub async fn load(
            fs: fs_bridge::JsFileSystemBridge,
            secret_key: &[u8],
        ) -> Result<WasmVault, JsError> {
            let key_bytes: [u8; 32] = secret_key
                .try_into()
                .map_err(|_| JsError::new("secret_key must be exactly 32 bytes"))?;
            let author = sync_core::PeerId::from_secret_bytes(key_bytes);

            let inner = sync_core::Vault::load(fs, author)
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
            let report = self
                .inner
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

        /// Get this device's Loro author PeerId (the id this replica authors under).
        #[wasm_bindgen(js_name = peerId)]
        pub fn peer_id(&self) -> String {
            self.inner.loro_author().to_string()
        }

        /// Get the VaultId (gossip topic seed + mDNS mesh grouping key).
        ///
        /// Shared across every replica of this vault — use this, not `peerId()`,
        /// to seed the gossip topic so all devices join the same mesh.
        #[wasm_bindgen(js_name = vaultId)]
        pub fn vault_id(&self) -> String {
            self.inner.vault_id().to_string()
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
                .map(|_| ())
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
            sync_core::Vault::<sync_core::fs::InMemoryFs>::version_includes(
                current_version,
                synced_version,
            )
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
            log(&format!(
                "processSyncMessage: received {} bytes",
                data.len()
            ));

            let (response, modified_paths) =
                self.inner.process_sync_message(data).await.map_err(|e| {
                    error(&format!("processSyncMessage error: {}", e));
                    JsError::new(&e.to_string())
                })?;

            log(&format!(
                "processSyncMessage: response={}, modified={:?}",
                response.as_ref().map(|r| r.len()).unwrap_or(0),
                modified_paths
            ));

            // Return as a JS object: { response: Uint8Array | null, modifiedPaths: string[] }
            let result = SyncMessageResult {
                response,
                modified_paths,
            };

            serde_wasm_bindgen::to_value(&result).map_err(|e| JsError::new(&e.to_string()))
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
        pub fn prepare_file_renamed(
            &self,
            old_path: &str,
            new_path: &str,
        ) -> Result<JsValue, JsError> {
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
            version
                .serialize(&serializer)
                .map_err(|e| JsError::new(&e.to_string()))
        }

        /// Get registry oplog statistics.
        ///
        /// Returns `{ changeCount, opCount }`.
        #[wasm_bindgen(js_name = getRegistryStats)]
        pub fn get_registry_stats(&self) -> Result<JsValue, JsError> {
            let stats = self.inner.get_registry_stats();
            serde_wasm_bindgen::to_value(&stats).map_err(|e| JsError::new(&e.to_string()))
        }

        /// Get cheap metadata from the .loro blob header.
        ///
        /// Returns blob metadata (version vectors, timestamps, change count) without
        /// loading the full document. Returns `null` if the document doesn't exist.
        #[wasm_bindgen(js_name = getDocumentBlobMeta)]
        pub async fn get_document_blob_meta(&self, path: &str) -> Result<JsValue, JsError> {
            let meta = self
                .inner
                .get_document_blob_meta(path)
                .await
                .map_err(|e| JsError::new(&e.to_string()))?;
            match meta {
                Some(m) => {
                    serde_wasm_bindgen::to_value(&m).map_err(|e| JsError::new(&e.to_string()))
                }
                None => Ok(JsValue::NULL),
            }
        }

        /// Get full document info (requires loading the document).
        ///
        /// Returns content metadata including body length, frontmatter status, and doc_id.
        /// Returns `null` if the document doesn't exist.
        #[wasm_bindgen(js_name = getDocumentInfo)]
        pub async fn get_document_info(&self, path: &str) -> Result<JsValue, JsError> {
            let info = self
                .inner
                .get_document_info(path)
                .await
                .map_err(|e| JsError::new(&e.to_string()))?;
            match info {
                Some(i) => {
                    serde_wasm_bindgen::to_value(&i).map_err(|e| JsError::new(&e.to_string()))
                }
                None => Ok(JsValue::NULL),
            }
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

    // ========== WasmSyncNode ==========

    /// Serializable gossip event for JS consumers.
    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase", tag = "type")]
    enum GossipEventJs {
        /// A peer joined the gossip swarm.
        NeighborUp { node_id: String },
        /// A peer left the gossip swarm.
        NeighborDown { node_id: String },
        /// A change notification received from a peer.
        ChangeReceived { from: String, path: String },
    }

    /// Serializable inbound sync request for JS consumers.
    #[derive(serde::Serialize)]
    #[serde(rename_all = "camelCase")]
    struct InboundSyncRequestJs {
        /// The encoded SyncMessage bytes to pass to WasmVault.processSyncMessage.
        message_bytes: Vec<u8>,
        /// The remote peer's PeerId (64-char hex string).
        remote_id: String,
    }

    /// iroh-based P2P sync node exposed to JavaScript.
    ///
    /// Wraps `SyncNode` and provides async polling methods for gossip events
    /// and inbound sync requests. The plugin drives these polling loops to
    /// receive peer events without needing explicit background threads.
    ///
    /// # Usage pattern
    ///
    /// ```js
    /// const node = await WasmSyncNode.create(secretKeyBytes);
    /// await node.joinVaultGossip(vaultId, bootstrapPeers);
    ///
    /// // Drive gossip events in a loop
    /// while (running) {
    ///     const event = await node.pollGossipEvent();
    ///     if (event) handleEvent(event);
    /// }
    /// ```
    #[wasm_bindgen]
    pub struct WasmSyncNode {
        inner: RefCell<Option<WasmSyncNodeState>>,
    }

    struct WasmSyncNodeState {
        node: sync_core::network::SyncNode,
        gossip: Option<sync_core::network::gossip::VaultGossip>,
        /// Pending reply sender for the current inbound sync request.
        /// The plugin calls `replyInboundSync` with raw response bytes to complete the exchange.
        pending_reply: Option<tokio::sync::oneshot::Sender<Vec<u8>>>,
    }

    #[wasm_bindgen]
    impl WasmSyncNode {
        /// Create a new iroh sync node from a 32-byte ed25519 secret key.
        ///
        /// `relay_url` — if provided, the node routes through that relay URL for peers
        /// that cannot be reached directly. Pass the daemon's embedded relay URL
        /// (from `DaemonStatus.relayUrl`) for reliable LAN and internet connectivity.
        /// If omitted, the node uses direct QUIC only.
        #[wasm_bindgen]
        pub async fn create(
            secret_key: &[u8],
            relay_url: Option<String>,
        ) -> Result<WasmSyncNode, JsError> {
            let key_bytes: [u8; 32] = secret_key
                .try_into()
                .map_err(|_| JsError::new("secret_key must be exactly 32 bytes"))?;

            let relay = relay_url
                .map(|u| u.parse::<iroh::RelayUrl>())
                .transpose()
                .map_err(|e| JsError::new(&format!("Invalid relay URL: {e}")))?;

            let node = sync_core::network::SyncNode::new(key_bytes, relay.as_ref())
                .await
                .map_err(|e| JsError::new(&format!("Failed to create sync node: {e}")))?;

            Ok(WasmSyncNode {
                inner: RefCell::new(Some(WasmSyncNodeState {
                    node,
                    gossip: None,
                    pending_reply: None,
                })),
            })
        }

        /// This node's iroh EndpointId as a hex string.
        ///
        /// This is the public key of the ed25519 keypair and uniquely identifies
        /// this node in the iroh network.
        #[wasm_bindgen(js_name = nodeId)]
        pub fn node_id(&self) -> Result<String, JsError> {
            let state = self.inner.borrow();
            let state = state
                .as_ref()
                .ok_or_else(|| JsError::new("Node is shut down"))?;
            Ok(state.node.node_id().to_string())
        }

        /// Join the gossip swarm for a specific vault.
        ///
        /// `vault_id` should be the vault's hex string identifier.
        /// `bootstrap_peers` is a JS array of hex string `EndpointId`s to bootstrap from.
        ///
        /// After calling this, use `pollGossipEvent` to receive membership and
        /// change notification events.
        #[wasm_bindgen(js_name = joinVaultGossip)]
        pub async fn join_vault_gossip(
            &self,
            vault_id: &str,
            bootstrap_peers: js_sys::Array,
        ) -> Result<(), JsError> {
            use iroh::EndpointId;
            use std::str::FromStr;

            // Parse vault ID from its 16-char hex string representation
            let vault_id: sync_core::peer_id::VaultId = vault_id
                .parse()
                .map_err(|e| JsError::new(&format!("Invalid vault_id: {e}")))?;

            // Parse bootstrap peer IDs
            let mut peers: Vec<EndpointId> = Vec::new();
            for peer_val in bootstrap_peers.iter() {
                let peer_str = peer_val
                    .as_string()
                    .ok_or_else(|| JsError::new("bootstrap_peers must be strings"))?;
                let peer_id = EndpointId::from_str(&peer_str)
                    .map_err(|e| JsError::new(&format!("Invalid peer id '{peer_str}': {e}")))?;
                peers.push(peer_id);
            }

            let mut state = self.inner.borrow_mut();
            let state = state
                .as_mut()
                .ok_or_else(|| JsError::new("Node is shut down"))?;

            let gossip = state
                .node
                .join_vault_gossip(&vault_id, peers)
                .await
                .map_err(|e| JsError::new(&format!("Failed to join vault gossip: {e}")))?;

            state.gossip = Some(gossip);
            Ok(())
        }

        /// Poll for the next gossip event (non-blocking).
        ///
        /// Returns a JS object if an event is immediately available, or `null` otherwise.
        /// The plugin should call this in a loop, yielding between calls (e.g., with
        /// `await Promise.resolve()`) to let the WASM event queue process incoming data.
        ///
        /// Event types:
        /// - `{ type: "neighborUp", nodeId: string }` — peer joined the swarm
        /// - `{ type: "neighborDown", nodeId: string }` — peer left the swarm
        /// - `{ type: "changeReceived", from: string, path: string }` — change notification
        #[wasm_bindgen(js_name = pollGossipEvent)]
        pub fn poll_gossip_event(&self) -> Result<JsValue, JsError> {
            use sync_core::network::gossip::GossipEvent;
            use tokio::sync::mpsc::error::TryRecvError;

            let mut state = self.inner.borrow_mut();
            let state = state
                .as_mut()
                .ok_or_else(|| JsError::new("Node is shut down"))?;
            let gossip = match state.gossip.as_mut() {
                Some(g) => g,
                None => return Ok(JsValue::NULL),
            };

            let event = match gossip.event_rx.try_recv() {
                Ok(event) => event,
                Err(TryRecvError::Empty) => return Ok(JsValue::NULL),
                Err(TryRecvError::Disconnected) => return Ok(JsValue::NULL),
            };

            let js_event = match event {
                GossipEvent::NeighborUp(id) => GossipEventJs::NeighborUp {
                    node_id: id.to_string(),
                },
                GossipEvent::NeighborDown(id) => GossipEventJs::NeighborDown {
                    node_id: id.to_string(),
                },
                GossipEvent::ChangeReceived { from, notification } => {
                    GossipEventJs::ChangeReceived {
                        from: from.to_string(),
                        path: notification.path,
                    }
                }
                // AllowlistUpdate is handled by the native daemon only.
                // The WASM plugin receives allowlist changes through the daemon's
                // storage layer rather than gossip.
                GossipEvent::AllowlistUpdate { .. } => return Ok(JsValue::NULL),
            };

            serde_wasm_bindgen::to_value(&js_event).map_err(|e| JsError::new(&e.to_string()))
        }

        /// Broadcast a change notification to all vault peers.
        ///
        /// The notification is lightweight (path only, ~1KB). Peers who receive it
        /// will open a QUIC stream to pull the actual sync data.
        #[wasm_bindgen(js_name = broadcastChange)]
        pub async fn broadcast_change(&self, path: &str) -> Result<(), JsError> {
            let mut state = self.inner.borrow_mut();
            let state = state
                .as_mut()
                .ok_or_else(|| JsError::new("Node is shut down"))?;
            let gossip = state
                .gossip
                .as_mut()
                .ok_or_else(|| JsError::new("Not joined to vault gossip"))?;
            gossip
                .broadcast_change(path)
                .await
                .map_err(|e| JsError::new(&format!("Broadcast failed: {e}")))
        }

        /// Poll for the next inbound sync request from a remote peer.
        ///
        /// Returns a JS object with `{ messageBytes: Uint8Array }`, or `null` if
        /// no request is available. Call `replyInboundSync` with the response bytes
        /// after processing the message.
        ///
        /// The plugin should pass `messageBytes` to `WasmVault.processSyncMessage`
        /// and then reply with the resulting `response` bytes.
        #[wasm_bindgen(js_name = pollInboundSync)]
        pub fn poll_inbound_sync(&self) -> Result<JsValue, JsError> {
            use tokio::sync::mpsc::error::TryRecvError;

            let mut state = self.inner.borrow_mut();
            let state = state
                .as_mut()
                .ok_or_else(|| JsError::new("Node is shut down"))?;

            match state.node.inbound_sync_rx.try_recv() {
                Ok(request) => {
                    // Raw bytes from the transport — pass directly to JS without re-serializing.
                    let message_bytes = request.message_bytes;
                    let remote_id = request.remote_id.to_string();

                    // Store the reply channel for `replyInboundSync`
                    state.pending_reply = Some(request.reply_tx);

                    let js_req = InboundSyncRequestJs {
                        message_bytes,
                        remote_id,
                    };
                    serde_wasm_bindgen::to_value(&js_req).map_err(|e| JsError::new(&e.to_string()))
                }
                Err(TryRecvError::Empty) => Ok(JsValue::NULL),
                Err(TryRecvError::Disconnected) => Ok(JsValue::NULL),
            }
        }

        /// Send a reply to the current inbound sync request.
        ///
        /// Call this after processing the sync message via `WasmVault.processSyncMessage`.
        /// Pass the `response` bytes from the process result (or omit if no response).
        #[wasm_bindgen(js_name = replyInboundSync)]
        pub fn reply_inbound_sync(&self, response_bytes: &[u8]) -> Result<(), JsError> {
            let mut state = self.inner.borrow_mut();
            let state = state
                .as_mut()
                .ok_or_else(|| JsError::new("Node is shut down"))?;

            let reply_tx = state
                .pending_reply
                .take()
                .ok_or_else(|| JsError::new("No pending inbound sync request"))?;

            // Pass raw bytes directly — the transport layer handles framing.
            // Ignore send errors — the connection may have closed.
            let _ = reply_tx.send(response_bytes.to_vec());
            Ok(())
        }

        /// Open a QUIC stream to a peer and perform a sync round-trip.
        ///
        /// `peer_id` is the hex string `EndpointId` of the peer to connect to.
        /// `request_bytes` are the serialized `SyncMessage` bytes to send.
        ///
        /// Returns the serialized response `SyncMessage` bytes.
        #[wasm_bindgen(js_name = syncWithPeer)]
        pub async fn sync_with_peer(
            &self,
            peer_id: &str,
            request_bytes: &[u8],
        ) -> Result<Vec<u8>, JsError> {
            use iroh::EndpointId;
            use std::str::FromStr;
            use sync_core::network::streams::connect_and_sync_raw;

            let peer = EndpointId::from_str(peer_id)
                .map_err(|e| JsError::new(&format!("Invalid peer_id: {e}")))?;

            // Borrow the endpoint for the sync call
            let endpoint = {
                let state = self.inner.borrow();
                let state = state
                    .as_ref()
                    .ok_or_else(|| JsError::new("Node is shut down"))?;
                state.node.endpoint.clone()
            };

            // Pass raw bytes directly — no serialization step needed here.
            connect_and_sync_raw(&endpoint, peer, request_bytes)
                .await
                .map_err(|e| JsError::new(&format!("Sync failed: {e}")))
        }

        /// Shut down the sync node, closing all connections.
        #[wasm_bindgen]
        pub async fn shutdown(&self) -> Result<(), JsError> {
            let state = self.inner.borrow_mut().take();
            if let Some(state) = state {
                state
                    .node
                    .shutdown()
                    .await
                    .map_err(|e| JsError::new(&format!("Shutdown failed: {e}")))?;
            }
            Ok(())
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
}

// Re-export wasm_impl contents at crate root for wasm32 targets
#[cfg(target_arch = "wasm32")]
pub use wasm_impl::*;
