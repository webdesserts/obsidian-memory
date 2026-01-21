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
    use wasm_bindgen::prelude::*;

    /// Initialize the WASM module (sets up panic hook for better error messages)
    #[wasm_bindgen]
    pub fn init() {
        console_error_panic_hook::set_once();
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
        pub async fn reconcile(&mut self) -> Result<JsValue, JsError> {
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
        pub async fn on_file_changed(&mut self, path: &str) -> Result<(), JsError> {
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
        pub async fn get_document_version(&mut self, path: &str) -> Result<JsValue, JsError> {
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
        pub async fn prepare_sync_request(&mut self) -> Result<Vec<u8>, JsError> {
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
        pub async fn process_sync_message(&mut self, data: &[u8]) -> Result<JsValue, JsError> {
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

            serde_wasm_bindgen::to_value(&result).map_err(|e| JsError::new(&e.to_string()))
        }

        /// Prepare a document update to broadcast after a local file change.
        ///
        /// Returns serialized bytes to send to all connected peers,
        /// or null if no update is needed.
        ///
        /// Call this after `onFileChanged` to get the update to broadcast.
        #[wasm_bindgen(js_name = prepareDocumentUpdate)]
        pub async fn prepare_document_update(&mut self, path: &str) -> Result<JsValue, JsError> {
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
        pub async fn delete_file(&mut self, path: &str) -> Result<(), JsError> {
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
        pub async fn rename_file(&mut self, old_path: &str, new_path: &str) -> Result<(), JsError> {
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
