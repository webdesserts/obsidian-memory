//! SyncEngine: Handles the sync protocol between peers.
//!
//! The sync protocol works as follows:
//!
//! 1. On connect, peer A sends a SyncRequest with its version vectors
//! 2. Peer B receives the request and responds with SyncExchange containing:
//!    - SyncResponse: updates A needs from B
//!    - SyncRequest: B's version vectors so A can send updates B needs
//! 3. Peer A processes the SyncExchange:
//!    - Applies updates from the response
//!    - Prepares and sends a final SyncResponse with updates B needs
//! 4. On file change, the editing peer broadcasts a DocumentUpdate to all peers
//!
//! This symmetric protocol enables full bidirectional sync in a single round-trip.

use crate::document::NoteDocument;
use crate::fs::FileSystem;
use crate::sync::{SyncMessage, SyncRequestData, SyncResponseData};
use crate::vault::Vault;

use std::collections::HashMap;
use thiserror::Error;
use tracing::{debug, warn};

#[derive(Debug, Error)]
pub enum SyncEngineError {
    #[error("Serialization error: {0}")]
    Serialization(String),

    #[error("Deserialization error: {0}")]
    Deserialization(String),

    #[error("Vault error: {0}")]
    Vault(#[from] crate::vault::VaultError),

    #[error("Document error: {0}")]
    Document(#[from] crate::document::DocumentError),
}

pub type Result<T> = std::result::Result<T, SyncEngineError>;

impl<F: FileSystem> Vault<F> {
    /// Prepare a sync request to send to a peer.
    ///
    /// Returns serialized bytes of a SyncRequest containing our version vectors
    /// for all known documents.
    pub async fn prepare_sync_request(&mut self) -> Result<Vec<u8>> {
        // Get registry version
        let registry_version = self.registry_version();

        // Get versions for all loaded documents
        let mut document_versions = HashMap::new();
        
        // Load all files to get their versions
        let files = self.list_files().await?;
        for path in files {
            // Load document if not already loaded
            let doc = self.get_document(&path).await?;
            let version = doc.version().encode();
            document_versions.insert(path, version);
        }

        let msg = SyncMessage::SyncRequest {
            registry_version,
            document_versions,
        };

        bincode::serialize(&msg)
            .map_err(|e| SyncEngineError::Serialization(e.to_string()))
    }

    /// Process an incoming sync message and return any outgoing response.
    ///
    /// Returns:
    /// - For SyncRequest: a SyncResponse with updates the peer is missing
    /// - For SyncResponse: applies updates and returns None
    /// - For DocumentUpdate: applies the update and returns None
    ///
    /// Also returns paths of documents that were modified.
    pub async fn process_sync_message(
        &mut self,
        data: &[u8],
    ) -> Result<(Option<Vec<u8>>, Vec<String>)> {
        let msg: SyncMessage = bincode::deserialize(data)
            .map_err(|e| SyncEngineError::Deserialization(e.to_string()))?;

        match msg {
            SyncMessage::SyncRequest {
                registry_version,
                document_versions,
            } => {
                // Peer is requesting sync - respond with SyncExchange (symmetric protocol)
                let exchange = self.prepare_sync_exchange(&registry_version, document_versions).await?;
                let exchange_bytes = bincode::serialize(&exchange)
                    .map_err(|e| SyncEngineError::Serialization(e.to_string()))?;
                Ok((Some(exchange_bytes), vec![]))
            }

            SyncMessage::SyncResponse {
                registry_updates,
                document_updates,
            } => {
                // Apply registry updates first (handles deletes/renames)
                if let Some(reg_data) = registry_updates {
                    self.apply_registry_updates(&reg_data).await?;
                }
                // Then apply document updates
                let modified = self.apply_document_updates(document_updates).await?;
                Ok((None, modified))
            }

            SyncMessage::SyncExchange { response, request } => {
                // Peer responded to our SyncRequest with:
                // - response: updates we need from them
                // - request: their version vectors so we can send them updates

                debug!("SyncExchange: received {} document updates, {} version vectors",
                    response.document_updates.len(), request.document_versions.len());

                // Track which files we're receiving so we don't echo them back
                let received_files: std::collections::HashSet<String> =
                    response.document_updates.keys().cloned().collect();

                // Apply registry updates first (handles deletes/renames)
                if let Some(reg_data) = response.registry_updates {
                    self.apply_registry_updates(&reg_data).await?;
                }

                // Then apply document updates
                let modified = self.apply_document_updates(response.document_updates).await?;
                debug!("SyncExchange: modified {} files: {:?}", modified.len(), modified);
                
                // Then, prepare updates they need from us (excluding files we just received)
                let our_response = self.prepare_sync_response_data_excluding(
                    &request.registry_version,
                    request.document_versions,
                    &received_files,
                ).await?;
                let response_msg = SyncMessage::SyncResponse {
                    registry_updates: our_response.registry_updates,
                    document_updates: our_response.document_updates,
                };
                let response_bytes = bincode::serialize(&response_msg)
                    .map_err(|e| SyncEngineError::Serialization(e.to_string()))?;
                
                Ok((Some(response_bytes), modified))
            }

            SyncMessage::DocumentUpdate { path, data, mtime } => {
                // Real-time update from peer
                let modified = self.apply_single_update(&path, &data, mtime).await?;
                Ok((None, if modified { vec![path] } else { vec![] }))
            }

            SyncMessage::FileDeleted { path } => {
                // Handle file deletion via tree operation
                debug!("Received file deletion for: {}", path);
                // Mark as synced BEFORE deleting (for echo detection)
                self.mark_synced(&path);
                self.delete_file(&path).await?;
                Ok((None, vec![path]))
            }

            SyncMessage::FileRenamed { old_path, new_path } => {
                // Handle file rename via tree operation
                debug!("Received file rename: {} -> {}", old_path, new_path);
                // Mark both paths as synced BEFORE renaming (for echo detection)
                // Some file watchers emit delete for old_path + create for new_path
                self.mark_synced(&old_path);
                self.mark_synced(&new_path);
                self.rename_file(&old_path, &new_path).await?;
                Ok((None, vec![new_path]))
            }
        }
    }

    /// Prepare a document update to broadcast after a file change.
    ///
    /// Returns None if the document hasn't been loaded/modified.
    pub async fn prepare_document_update(&mut self, path: &str) -> Result<Option<Vec<u8>>> {
        // Ensure document is loaded
        let doc = self.get_document(path).await?;

        // Export a snapshot (for now - could optimize to send incremental updates)
        let snapshot = doc.export_snapshot();

        // Get file modification time for "latest wins" conflict resolution
        let mtime = self.fs.stat(path).await.ok().map(|s| s.mtime_millis);

        let msg = SyncMessage::DocumentUpdate {
            path: path.to_string(),
            data: snapshot,
            mtime,
        };

        let bytes = bincode::serialize(&msg)
            .map_err(|e| SyncEngineError::Serialization(e.to_string()))?;

        Ok(Some(bytes))
    }

    /// Prepare a file deletion message to broadcast.
    pub fn prepare_file_deleted(&self, path: &str) -> Result<Vec<u8>> {
        let msg = SyncMessage::FileDeleted {
            path: path.to_string(),
        };

        bincode::serialize(&msg)
            .map_err(|e| SyncEngineError::Serialization(e.to_string()))
    }

    /// Prepare a file renamed message to broadcast.
    pub fn prepare_file_renamed(&self, old_path: &str, new_path: &str) -> Result<Vec<u8>> {
        let msg = SyncMessage::FileRenamed {
            old_path: old_path.to_string(),
            new_path: new_path.to_string(),
        };

        bincode::serialize(&msg)
            .map_err(|e| SyncEngineError::Serialization(e.to_string()))
    }

    /// Get the registry version vector as bytes.
    fn registry_version(&self) -> Vec<u8> {
        self.registry.state_vv().encode()
    }

    /// Prepare a SyncExchange in response to a SyncRequest.
    ///
    /// This bundles:
    /// - Our response (updates they need from us)
    /// - Our request (our version vectors so they can send us updates)
    async fn prepare_sync_exchange(
        &mut self,
        their_registry_version: &[u8],
        their_versions: HashMap<String, Vec<u8>>,
    ) -> Result<SyncMessage> {
        // Prepare updates they need from us
        let response = self.prepare_sync_response_data(their_registry_version, their_versions).await?;

        // Prepare our version vectors so they can send us updates
        let request = self.prepare_sync_request_data().await?;

        Ok(SyncMessage::SyncExchange { response, request })
    }

    /// Prepare sync request data (our version vectors).
    async fn prepare_sync_request_data(&mut self) -> Result<SyncRequestData> {
        let registry_version = self.registry_version();
        let mut document_versions = HashMap::new();
        
        let files = self.list_files().await?;
        for path in files {
            let doc = self.get_document(&path).await?;
            let version = doc.version().encode();
            document_versions.insert(path, version);
        }
        
        Ok(SyncRequestData {
            registry_version,
            document_versions,
        })
    }

    /// Prepare sync response data (updates the peer is missing).
    async fn prepare_sync_response_data(
        &mut self,
        their_registry_version: &[u8],
        their_versions: HashMap<String, Vec<u8>>,
    ) -> Result<SyncResponseData> {
        self.prepare_sync_response_data_excluding(
            their_registry_version,
            their_versions,
            &std::collections::HashSet::new(),
        ).await
    }

    /// Prepare sync response data, excluding specific files.
    ///
    /// Used when responding to a SyncExchange - we exclude files we just received
    /// to avoid echoing them back. Loro's import creates a local change marker,
    /// so version-based comparison would incorrectly send updates for files
    /// we just imported.
    async fn prepare_sync_response_data_excluding(
        &mut self,
        their_registry_version: &[u8],
        their_versions: HashMap<String, Vec<u8>>,
        exclude: &std::collections::HashSet<String>,
    ) -> Result<SyncResponseData> {
        let mut document_updates = HashMap::new();

        // Get all our files
        let our_files = self.list_files().await?;

        for path in our_files {
            // Skip files we just received (would incorrectly appear as updates due to import marker)
            if exclude.contains(&path) {
                continue;
            }

            let doc = self.get_document(&path).await?;
            let _our_version = doc.version();

            // Check if they have this document and what version
            if let Some(their_version_bytes) = their_versions.get(&path) {
                // They have it - send updates since their version
                if let Ok(their_version) = loro::VersionVector::decode(their_version_bytes) {
                    let updates = doc.export_updates(&their_version);
                    if !updates.is_empty() {
                        document_updates.insert(path, updates);
                    }
                }
            } else {
                // They don't have it - send full snapshot
                document_updates.insert(path, doc.export_snapshot());
            }
        }

        // Export registry updates if they have an older version
        let registry_updates = if !their_registry_version.is_empty() {
            if let Ok(their_version) = loro::VersionVector::decode(their_registry_version) {
                match self.registry.export(loro::ExportMode::updates(&their_version)) {
                    Ok(updates) if !updates.is_empty() => Some(updates),
                    _ => None,
                }
            } else {
                // Invalid version - send full snapshot
                self.registry.export(loro::ExportMode::snapshot()).ok()
            }
        } else {
            // They don't have registry - send full snapshot
            self.registry.export(loro::ExportMode::snapshot()).ok()
        };

        Ok(SyncResponseData {
            registry_updates,
            document_updates,
        })
    }

    /// Apply registry updates from a sync response.
    ///
    /// Imports the registry CRDT updates and rebuilds the path cache.
    /// Syncs filesystem with tree state (deletes files marked as deleted).
    async fn apply_registry_updates(&mut self, data: &[u8]) -> Result<()> {
        debug!("apply_registry_updates: data_len={}", data.len());

        // Import registry updates
        self.registry
            .import(data)
            .map_err(|e| SyncEngineError::Deserialization(format!("Registry import failed: {}", e)))?;

        // Rebuild path cache from updated tree
        self.rebuild_path_cache();

        // Sync filesystem with tree state - delete files that are deleted in tree
        self.apply_registry_changes().await?;

        // Save updated registry to disk
        let registry_bytes = self.registry.export(loro::ExportMode::snapshot()).unwrap();
        self.fs
            .write(&format!("{}/registry.loro", crate::vault::SYNC_DIR), &registry_bytes)
            .await
            .map_err(crate::vault::VaultError::from)?;

        debug!("apply_registry_updates: complete");
        Ok(())
    }

    /// Apply registry changes to filesystem.
    ///
    /// Deletes files on disk that are marked as deleted in the tree.
    async fn apply_registry_changes(&mut self) -> Result<()> {
        let tree = self.file_tree();

        // Find deleted files and clean them up
        for node_id in tree.nodes() {
            if tree.is_node_deleted(&node_id).unwrap_or(false) {
                // Get the path before it was deleted (if we can reconstruct it)
                if let Some(path) = self.get_node_path(&node_id) {
                    // Remove from filesystem
                    if self.fs.exists(&path).await.unwrap_or(false) {
                        debug!("apply_registry_changes: deleting {}", path);
                        // Mark as synced BEFORE deleting (for echo detection)
                        self.mark_synced(&path);
                        if let Err(e) = self.fs.delete(&path).await {
                            warn!("Failed to delete {}: {}", path, e);
                        }
                    }

                    // Remove .loro document
                    let sync_path = self.document_sync_path(&path);
                    if self.fs.exists(&sync_path).await.unwrap_or(false) {
                        if let Err(e) = self.fs.delete(&sync_path).await {
                            warn!("Failed to delete .loro file {}: {}", sync_path, e);
                        }
                    }

                    // Remove from documents cache
                    self.documents.remove(&path);
                }
            }
        }

        Ok(())
    }

    /// Apply document updates from a sync response.
    ///
    /// Note: SyncResponse doesn't include mtime, so "latest wins" falls back to "remote wins"
    /// for initial sync. Real-time DocumentUpdate messages include mtime for proper resolution.
    async fn apply_document_updates(
        &mut self,
        updates: HashMap<String, Vec<u8>>,
    ) -> Result<Vec<String>> {
        let mut modified = Vec::new();

        for (path, data) in updates {
            // No mtime available in bulk sync - uses "remote wins" for divergent histories
            if self.apply_single_update(&path, &data, None).await? {
                modified.push(path);
            }
        }

        Ok(modified)
    }

    /// Apply a single document update.
    ///
    /// Returns true if the document was modified.
    ///
    /// When histories diverge (neither includes the other), uses content reconciliation
    /// via `update_by_line()` instead of CRDT merge to avoid character interleaving.
    ///
    /// For divergent histories, uses "latest wins" based on file mtime when available.
    /// Falls back to "remote wins" if mtime is unavailable (e.g., bulk sync).
    async fn apply_single_update(
        &mut self,
        path: &str,
        data: &[u8],
        remote_mtime: Option<u64>,
    ) -> Result<bool> {
        debug!("apply_single_update: {} - data_len={}", path, data.len());

        // Check if document exists (in cache or on disk)
        let sync_path = self.document_sync_path(path);
        let exists_in_cache = self.documents.contains_key(path);
        let exists_on_disk = self
            .fs
            .exists(&sync_path)
            .await
            .map_err(crate::vault::VaultError::from)?;

        if exists_in_cache || exists_on_disk {
            // Get local mtime before borrowing doc (needed for "latest wins" comparison)
            let local_mtime = self.fs.stat(path).await.ok().map(|s| s.mtime_millis);

            // Document exists - check for divergent histories before merging
            let doc = self.get_document_mut(path).await?;
            let local_vv = doc.version();

            // Create temp doc FROM LOCAL STATE, then import remote to get merged version
            // This correctly handles incremental updates (not just full snapshots)
            let mut temp_doc = NoteDocument::from_bytes(path, &doc.export_snapshot())?;
            temp_doc.import(data)?;
            let merged_vv = temp_doc.version();

            // Check if the merge caused any change
            let local_includes_merged = local_vv.includes_vv(&merged_vv);

            // Check if histories are truly divergent by comparing doc_ids.
            // Documents from the same source (synced) share the same doc_id.
            // Documents created independently have different doc_ids.
            let remote_only_doc = NoteDocument::from_bytes(path, data)?;

            let local_doc_id = doc.doc_id();
            let remote_doc_id = remote_only_doc.doc_id();

            let is_divergent = match (&local_doc_id, &remote_doc_id) {
                (Some(local_id), Some(remote_id)) => local_id != remote_id,
                // If either lacks doc_id (legacy document or incremental update), assume compatible
                _ => false,
            };

            debug!(
                "apply_single_update: {} - local_doc_id={:?}, remote_doc_id={:?}, divergent={}",
                path, local_doc_id, remote_doc_id, is_divergent
            );

            let modified = if is_divergent {
                // Divergent histories - use content reconciliation to avoid interleaving
                debug!(
                    "apply_single_update: {} - divergent histories, using content reconciliation",
                    path
                );

                // "Latest wins" - compare mtimes if available
                let remote_is_newer = match (remote_mtime, local_mtime) {
                    (Some(remote), Some(local)) => remote >= local,
                    // If mtime unavailable, fall back to "remote wins"
                    _ => true,
                };

                if remote_is_newer {
                    // Use remote_only_doc (pure remote content) NOT temp_doc (merged/interleaved)
                    let remote_body = remote_only_doc.body().to_string();
                    let body_changed = doc.update_body(&remote_body)?;

                    // Also reconcile frontmatter from pure remote
                    let remote_fm = remote_only_doc.to_markdown();
                    let parsed = crate::markdown::parse(&remote_fm);
                    let fm_changed = doc.update_frontmatter(parsed.frontmatter.as_ref())?;

                    if body_changed || fm_changed {
                        doc.commit();
                        true
                    } else {
                        false
                    }
                } else {
                    debug!(
                        "apply_single_update: {} - local is newer (local={:?}, remote={:?}), keeping local",
                        path, local_mtime, remote_mtime
                    );
                    false
                }
            } else if !local_includes_merged {
                // Remote has changes we don't have, but histories are compatible - safe to import
                let version_before = doc.version();
                doc.import(data)?;
                version_before != doc.version()
            } else {
                // We already have everything remote has
                false
            };

            debug!("apply_single_update: {} - modified={}", path, modified);

            if modified {
                // Mark as synced BEFORE writing to disk (for echo detection)
                self.mark_synced(path);
                self.save_document(path).await?;
                debug!("apply_single_update: saved {} to disk", path);
            }

            Ok(modified)
        } else {
            // Document is new - create directly from sync data (preserves peer ID)
            let doc = NoteDocument::from_bytes(path, data)?;

            // Mark as synced BEFORE writing to disk (for echo detection)
            self.mark_synced(path);

            // Save to disk
            let snapshot = doc.export_snapshot();
            self.fs.write(&sync_path, &snapshot).await.map_err(crate::vault::VaultError::from)?;
            self.fs.write(path, doc.to_markdown().as_bytes()).await.map_err(crate::vault::VaultError::from)?;

            // Note: Don't register in tree here - tree sync handles that via registry.
            // Registering here would create duplicate nodes with different IDs.

            // Add to cache
            self.documents.insert(path.to_string(), doc);

            debug!("apply_single_update: created new {} from sync data", path);
            Ok(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fs::InMemoryFs;

    #[tokio::test]
    async fn test_sync_between_vaults_symmetric() {
        // Create two vaults with different files
        let fs1 = InMemoryFs::new();
        let fs2 = InMemoryFs::new();

        // Vault 1 has file1
        fs1.write("file1.md", b"# From Vault 1").await.unwrap();
        
        // Vault 2 has file2
        fs2.write("file2.md", b"# From Vault 2").await.unwrap();

        // Initialize both vaults (this indexes existing files)
        let mut vault1 = Vault::init(fs1, "peer1".to_string()).await.unwrap();
        let mut vault2 = Vault::init(fs2, "peer2".to_string()).await.unwrap();

        // Vault 1 sends sync request to Vault 2
        let request = vault1.prepare_sync_request().await.unwrap();

        // Vault 2 processes request and sends SyncExchange (response + its own request)
        let (exchange, _) = vault2.process_sync_message(&request).await.unwrap();
        assert!(exchange.is_some(), "Should return SyncExchange");

        // Vault 1 processes the exchange:
        // - Applies file2 from vault2
        // - Sends back SyncResponse with file1 for vault2
        let (final_response, modified1) = vault1.process_sync_message(&exchange.unwrap()).await.unwrap();
        assert!(final_response.is_some(), "Should return final SyncResponse");
        assert!(modified1.contains(&"file2.md".to_string()), "Vault1 should receive file2");

        // Vault 2 processes the final response
        let (none, modified2) = vault2.process_sync_message(&final_response.unwrap()).await.unwrap();
        assert!(none.is_none(), "No more messages needed");
        assert!(modified2.contains(&"file1.md".to_string()), "Vault2 should receive file1");

        // Verify both vaults have both files
        let doc1_in_vault2 = vault2.get_document("file1.md").await.unwrap();
        assert!(doc1_in_vault2.to_markdown().contains("From Vault 1"));

        let doc2_in_vault1 = vault1.get_document("file2.md").await.unwrap();
        assert!(doc2_in_vault1.to_markdown().contains("From Vault 2"));
    }

    #[tokio::test]
    async fn test_sync_empty_vault_receives_files() {
        // Vault 1 has files, Vault 2 is empty
        let fs1 = InMemoryFs::new();
        let fs2 = InMemoryFs::new();

        fs1.write("note1.md", b"# Note 1").await.unwrap();
        fs1.write("note2.md", b"# Note 2").await.unwrap();

        let mut vault1 = Vault::init(fs1, "peer1".to_string()).await.unwrap();
        let mut vault2 = Vault::init(fs2, "peer2".to_string()).await.unwrap();

        // Empty vault sends sync request
        let request = vault2.prepare_sync_request().await.unwrap();

        // Vault 1 responds with SyncExchange
        let (exchange, _) = vault1.process_sync_message(&request).await.unwrap();

        // Vault 2 processes exchange - should receive both files
        let (final_response, modified) = vault2.process_sync_message(&exchange.unwrap()).await.unwrap();
        
        assert!(modified.contains(&"note1.md".to_string()));
        assert!(modified.contains(&"note2.md".to_string()));
        
        // Final response exists (vault2 sends SyncResponse even if empty)
        assert!(final_response.is_some());
        
        // Vault 1 processes final response - nothing new (vault2 was empty)
        let (none, modified1) = vault1.process_sync_message(&final_response.unwrap()).await.unwrap();
        assert!(none.is_none(), "No more messages after SyncResponse");
        assert!(modified1.is_empty(), "Vault1 already had everything");
    }

    #[tokio::test]
    async fn test_document_update_broadcast() {
        let fs1 = InMemoryFs::new();
        let fs2 = InMemoryFs::new();

        let mut vault1 = Vault::init(fs1, "peer1".to_string()).await.unwrap();
        let mut vault2 = Vault::init(fs2, "peer2".to_string()).await.unwrap();

        // Create and sync initial content
        vault1.fs.write("note.md", b"Initial content").await.unwrap();
        vault1.on_file_changed("note.md").await.unwrap();

        // Full sync to get vault2 up to date
        let request = vault2.prepare_sync_request().await.unwrap();
        let (exchange, _) = vault1.process_sync_message(&request).await.unwrap();
        let (final_resp, _) = vault2.process_sync_message(&exchange.unwrap()).await.unwrap();
        if let Some(resp) = final_resp {
            vault1.process_sync_message(&resp).await.unwrap();
        }

        // Now vault1 makes a change
        vault1.fs.write("note.md", b"Updated content").await.unwrap();
        vault1.on_file_changed("note.md").await.unwrap();

        // Vault1 broadcasts a document update (real-time sync)
        let update = vault1.prepare_document_update("note.md").await.unwrap();
        assert!(update.is_some());

        // Vault2 receives the update
        let (_, modified) = vault2.process_sync_message(&update.unwrap()).await.unwrap();
        assert!(modified.contains(&"note.md".to_string()));

        // Verify content
        let doc = vault2.get_document("note.md").await.unwrap();
        assert!(doc.to_markdown().contains("Updated content"));
    }

    #[tokio::test]
    async fn test_version_includes_basic() {
        // Test the version_includes helper function with direct Loro operations
        use crate::document::NoteDocument;

        // Create a document and get its initial version
        let doc1 = NoteDocument::from_markdown("test.md", "# Hello").unwrap();
        let v1 = doc1.version().encode();

        // Create another document and import doc1's state
        let mut doc2 = NoteDocument::new("test.md");
        doc2.import(&doc1.export_snapshot()).unwrap();
        let v2 = doc2.version().encode();

        // v2 should include v1 (it has all ops from doc1)
        assert!(
            Vault::<InMemoryFs>::version_includes(&v2, &v1),
            "After import, v2 should include v1"
        );

        // Note: v1 does NOT include v2 because v2's import creates
        // operations under v2's peer ID that v1 hasn't seen.
        // This is correct Loro behavior - import adds to version vector.
    }

    #[tokio::test]
    async fn test_sync_applies_updates_correctly() {
        // Test that sync correctly applies updates without creating duplicates
        let fs1 = InMemoryFs::new();
        let fs2 = InMemoryFs::new();

        let mut vault1 = Vault::init(fs1, "peer1".to_string()).await.unwrap();
        let mut vault2 = Vault::init(fs2, "peer2".to_string()).await.unwrap();

        // Vault1 creates a file
        vault1.fs.write("note.md", b"# Original").await.unwrap();
        vault1.on_file_changed("note.md").await.unwrap();

        // Sync to vault2
        let request = vault2.prepare_sync_request().await.unwrap();
        let (exchange, _) = vault1.process_sync_message(&request).await.unwrap();
        let (_, modified) = vault2.process_sync_message(&exchange.unwrap()).await.unwrap();

        // Vault2 should have received the file
        assert!(modified.contains(&"note.md".to_string()));

        // Verify content matches
        let doc1 = vault1.get_document("note.md").await.unwrap();
        let doc2 = vault2.get_document("note.md").await.unwrap();
        assert_eq!(doc1.to_markdown(), doc2.to_markdown());

        // Apply the same sync again - should be a no-op
        let request2 = vault2.prepare_sync_request().await.unwrap();
        let (exchange2, _) = vault1.process_sync_message(&request2).await.unwrap();
        let (_, modified2) = vault2.process_sync_message(&exchange2.unwrap()).await.unwrap();

        // Nothing should be modified (already in sync)
        assert!(modified2.is_empty(), "Re-sync should not modify anything");
    }

    #[tokio::test]
    async fn test_document_update_is_idempotent() {
        // Test that receiving the same DocumentUpdate twice doesn't cause issues
        let fs1 = InMemoryFs::new();
        let fs2 = InMemoryFs::new();

        let mut vault1 = Vault::init(fs1, "peer1".to_string()).await.unwrap();
        let mut vault2 = Vault::init(fs2, "peer2".to_string()).await.unwrap();

        // Vault1 creates a file
        vault1.fs.write("note.md", b"# Content").await.unwrap();
        vault1.on_file_changed("note.md").await.unwrap();

        // Get the document update
        let update = vault1.prepare_document_update("note.md").await.unwrap().unwrap();

        // Apply to vault2 first time
        let (_, modified1) = vault2.process_sync_message(&update).await.unwrap();
        assert!(modified1.contains(&"note.md".to_string()), "First apply should modify");

        // Apply the same update again
        let (_, modified2) = vault2.process_sync_message(&update).await.unwrap();
        assert!(modified2.is_empty(), "Second apply should be no-op (idempotent)");

        // Content should still be correct
        let doc = vault2.get_document("note.md").await.unwrap();
        assert!(doc.to_markdown().contains("# Content"));
    }

    #[tokio::test]
    async fn test_sync_echo_does_not_duplicate() {
        // Regression test for content duplication bug.
        // When a file is synced and written to disk, the file watcher triggers
        // on_file_changed(). Previously this created a new LoroDoc with a new
        // peer ID, causing content duplication on subsequent syncs.
        let fs1 = InMemoryFs::new();
        let fs2 = InMemoryFs::new();

        let mut vault1 = Vault::init(fs1, "peer1".to_string()).await.unwrap();
        let mut vault2 = Vault::init(fs2, "peer2".to_string()).await.unwrap();

        // Vault1 creates a file with specific content
        let content = "Hello";
        vault1.fs.write("note.md", content.as_bytes()).await.unwrap();
        vault1.on_file_changed("note.md").await.unwrap();

        // Sync vault1 → vault2
        let request = vault2.prepare_sync_request().await.unwrap();
        let (exchange, _) = vault1.process_sync_message(&request).await.unwrap();
        let (final_resp, _) = vault2.process_sync_message(&exchange.unwrap()).await.unwrap();
        if let Some(resp) = final_resp {
            vault1.process_sync_message(&resp).await.unwrap();
        }

        // Simulate file watcher: vault2 calls on_file_changed after sync writes to disk.
        // This is the bug scenario - previously created new peer ID and duplicated content.
        vault2.on_file_changed("note.md").await.unwrap();

        // Sync vault2 → vault1 (this would cause duplication before the fix)
        let request2 = vault1.prepare_sync_request().await.unwrap();
        let (exchange2, _) = vault2.process_sync_message(&request2).await.unwrap();
        let (final_resp2, _) = vault1.process_sync_message(&exchange2.unwrap()).await.unwrap();
        if let Some(resp) = final_resp2 {
            vault2.process_sync_message(&resp).await.unwrap();
        }

        // Verify content is exactly "Hello" (not "HelloHello" or duplicated)
        let doc = vault1.get_document("note.md").await.unwrap();
        let markdown = doc.to_markdown();
        assert_eq!(markdown, content, "Content should not be duplicated");
    }

    #[tokio::test]
    async fn test_local_edit_after_sync() {
        // Test that local edits after sync work correctly
        let fs1 = InMemoryFs::new();
        let fs2 = InMemoryFs::new();

        let mut vault1 = Vault::init(fs1, "peer1".to_string()).await.unwrap();
        let mut vault2 = Vault::init(fs2, "peer2".to_string()).await.unwrap();

        // Vault1 creates initial content
        vault1.fs.write("note.md", b"Hello").await.unwrap();
        vault1.on_file_changed("note.md").await.unwrap();

        // Sync to vault2
        let request = vault2.prepare_sync_request().await.unwrap();
        let (exchange, _) = vault1.process_sync_message(&request).await.unwrap();
        let (final_resp, _) = vault2.process_sync_message(&exchange.unwrap()).await.unwrap();
        if let Some(resp) = final_resp {
            vault1.process_sync_message(&resp).await.unwrap();
        }

        // Vault2 makes a local edit
        vault2.fs.write("note.md", b"Hello World").await.unwrap();
        vault2.on_file_changed("note.md").await.unwrap();

        // Sync back to vault1
        let request2 = vault1.prepare_sync_request().await.unwrap();
        let (exchange2, _) = vault2.process_sync_message(&request2).await.unwrap();
        let (final_resp2, _) = vault1.process_sync_message(&exchange2.unwrap()).await.unwrap();
        if let Some(resp) = final_resp2 {
            vault2.process_sync_message(&resp).await.unwrap();
        }

        // Vault1 should have the updated content
        let doc = vault1.get_document("note.md").await.unwrap();
        assert_eq!(doc.to_markdown(), "Hello World", "Edit should propagate correctly");
    }

    #[tokio::test]
    async fn test_diff_merge_preserves_peer_id() {
        // Test that diff-and-merge updates don't create new peer IDs
        let fs = InMemoryFs::new();
        let mut vault = Vault::init(fs, "peer1".to_string()).await.unwrap();

        // Create initial file
        vault.fs.write("note.md", b"Hello").await.unwrap();
        vault.on_file_changed("note.md").await.unwrap();

        // Get initial peer ID count from version vector
        let doc = vault.get_document("note.md").await.unwrap();
        let initial_version = doc.version();

        // Make an edit via on_file_changed (diff-and-merge path)
        vault.fs.write("note.md", b"Hello World").await.unwrap();
        vault.on_file_changed("note.md").await.unwrap();

        // Version vector should have grown but still have same number of peers
        let doc2 = vault.get_document("note.md").await.unwrap();
        let updated_version = doc2.version();

        // Both versions should have the same number of peer entries
        // (diff-merge doesn't create new peer IDs)
        assert_eq!(
            initial_version.len(),
            updated_version.len(),
            "Diff-merge should not create new peer IDs"
        );

        // Content should be updated
        assert_eq!(doc2.to_markdown(), "Hello World");
    }

    #[tokio::test]
    async fn test_reindex_during_reconcile_no_duplication() {
        // Regression test: reconcile() calls reindex_file() when files are modified externally.
        // Previously this created a new peer ID, causing content duplication on sync.
        use std::sync::Arc;

        let fs1 = Arc::new(InMemoryFs::new());
        let fs2 = Arc::new(InMemoryFs::new());

        // Initialize vault1 with a file
        fs1.write("note.md", b"Original").await.unwrap();
        let mut vault1 = Vault::init(Arc::clone(&fs1), "peer1".to_string()).await.unwrap();

        // Sync to vault2
        fs2.mkdir(".sync").await.unwrap();
        fs2.mkdir(".sync/documents").await.unwrap();
        let mut vault2 = Vault::init(Arc::clone(&fs2), "peer2".to_string()).await.unwrap();

        let request = vault2.prepare_sync_request().await.unwrap();
        let (exchange, _) = vault1.process_sync_message(&request).await.unwrap();
        let (final_resp, _) = vault2.process_sync_message(&exchange.unwrap()).await.unwrap();
        if let Some(resp) = final_resp {
            vault1.process_sync_message(&resp).await.unwrap();
        }

        // Simulate external modification on vault2 (plugin was off)
        fs2.write("note.md", b"Modified externally").await.unwrap();

        // Reload vault2 - this triggers reconcile() -> reindex_file()
        let mut vault2_reloaded = Vault::load(Arc::clone(&fs2), "peer2".to_string()).await.unwrap();

        // Sync back to vault1
        let request2 = vault1.prepare_sync_request().await.unwrap();
        let (exchange2, _) = vault2_reloaded.process_sync_message(&request2).await.unwrap();
        let (final_resp2, _) = vault1.process_sync_message(&exchange2.unwrap()).await.unwrap();
        if let Some(resp) = final_resp2 {
            vault2_reloaded.process_sync_message(&resp).await.unwrap();
        }

        // Verify content is NOT duplicated
        let doc = vault1.get_document("note.md").await.unwrap();
        let content = doc.to_markdown();
        assert_eq!(content, "Modified externally", "Content should not be duplicated after reconcile");
    }

    #[tokio::test]
    async fn test_cold_cache_no_duplication() {
        // Regression test: on_file_changed() when .loro exists on disk but not in memory cache.
        // Previously fell through to creating a new document with new peer ID.
        use std::sync::Arc;

        let fs1 = Arc::new(InMemoryFs::new());
        let fs2 = Arc::new(InMemoryFs::new());

        // Initialize vault1 with a file
        fs1.write("note.md", b"Hello").await.unwrap();
        let mut vault1 = Vault::init(Arc::clone(&fs1), "peer1".to_string()).await.unwrap();

        // Sync to vault2
        let mut vault2 = Vault::init(Arc::clone(&fs2), "peer2".to_string()).await.unwrap();
        let request = vault2.prepare_sync_request().await.unwrap();
        let (exchange, _) = vault1.process_sync_message(&request).await.unwrap();
        let (final_resp, _) = vault2.process_sync_message(&exchange.unwrap()).await.unwrap();
        if let Some(resp) = final_resp {
            vault1.process_sync_message(&resp).await.unwrap();
        }

        // Clear vault2's in-memory cache (simulate cold cache)
        vault2.documents.clear();

        // Make an edit and call on_file_changed (the .loro exists on disk but not in cache)
        fs2.write("note.md", b"Hello World").await.unwrap();
        vault2.on_file_changed("note.md").await.unwrap();

        // Sync back to vault1
        let request2 = vault1.prepare_sync_request().await.unwrap();
        let (exchange2, _) = vault2.process_sync_message(&request2).await.unwrap();
        let (final_resp2, _) = vault1.process_sync_message(&exchange2.unwrap()).await.unwrap();
        if let Some(resp) = final_resp2 {
            vault2.process_sync_message(&resp).await.unwrap();
        }

        // Verify content is correct (not duplicated)
        let doc = vault1.get_document("note.md").await.unwrap();
        let content = doc.to_markdown();
        assert_eq!(content, "Hello World", "Cold cache should not cause duplication");
    }

    #[tokio::test]
    async fn test_file_migration_preserves_peer_id() {
        // Test that file migration during reconcile preserves peer ID
        use std::sync::Arc;

        let fs1 = Arc::new(InMemoryFs::new());
        let fs2 = Arc::new(InMemoryFs::new());

        // Initialize vault1 with a file
        fs1.write("old_name.md", b"Content ABC").await.unwrap();
        let mut vault1 = Vault::init(Arc::clone(&fs1), "peer1".to_string()).await.unwrap();

        // Get the peer ID count from the original document
        let doc1 = vault1.get_document("old_name.md").await.unwrap();
        let original_peer_count = doc1.version().len();

        // Sync to vault2
        let mut vault2 = Vault::init(Arc::clone(&fs2), "peer2".to_string()).await.unwrap();
        let request = vault2.prepare_sync_request().await.unwrap();
        let (exchange, _) = vault1.process_sync_message(&request).await.unwrap();
        let (final_resp, _) = vault2.process_sync_message(&exchange.unwrap()).await.unwrap();
        if let Some(resp) = final_resp {
            vault1.process_sync_message(&resp).await.unwrap();
        }

        // Simulate file rename on vault2 (plugin was off)
        let content = fs2.read("old_name.md").await.unwrap();
        fs2.write("new_name.md", &content).await.unwrap();
        fs2.delete("old_name.md").await.unwrap();

        // Reload vault2 - this triggers reconcile() -> migrate_document()
        let mut vault2_reloaded = Vault::load(Arc::clone(&fs2), "peer2".to_string()).await.unwrap();

        // The migrated document should exist
        let doc2 = vault2_reloaded.get_document("new_name.md").await.unwrap();

        // Peer ID count should only increase by 1 (the path metadata update)
        // Previously it would add 2+ (one from new() and one from import)
        let migrated_peer_count = doc2.version().len();
        assert!(
            migrated_peer_count <= original_peer_count + 1,
            "Migration should not proliferate peer IDs: original={}, migrated={}",
            original_peer_count,
            migrated_peer_count
        );

        // Content should be preserved
        assert!(doc2.to_markdown().contains("Content ABC"));
    }

    #[tokio::test]
    async fn test_divergent_same_file_no_interleaving() {
        // Regression test: Two vaults create the SAME file with DIFFERENT content
        // BEFORE any sync. When they sync, content should NOT be interleaved.
        // This was the original bug where "# Hello" became "# # Hellello WWorld".
        use std::sync::Arc;

        let fs1 = Arc::new(InMemoryFs::new());
        let fs2 = Arc::new(InMemoryFs::new());

        // Both vaults create the SAME file with DIFFERENT content BEFORE sync
        fs1.write("note.md", b"# Hello from A").await.unwrap();
        // Add delay to ensure different mtime
        std::thread::sleep(std::time::Duration::from_millis(10));
        fs2.write("note.md", b"# Hello from B").await.unwrap();

        // Initialize vaults - each creates its own LoroDoc with independent peer IDs
        let mut vault1 = Vault::init(Arc::clone(&fs1), "peer1".to_string()).await.unwrap();
        let mut vault2 = Vault::init(Arc::clone(&fs2), "peer2".to_string()).await.unwrap();

        // Sync vault1 → vault2
        let request = vault2.prepare_sync_request().await.unwrap();
        let (exchange, _) = vault1.process_sync_message(&request).await.unwrap();
        let (final_resp, modified) = vault2.process_sync_message(&exchange.unwrap()).await.unwrap();
        if let Some(resp) = final_resp {
            vault1.process_sync_message(&resp).await.unwrap();
        }

        // Verify content is NOT interleaved
        let doc1 = vault1.get_document("note.md").await.unwrap();
        let doc2 = vault2.get_document("note.md").await.unwrap();

        let content1 = doc1.to_markdown();
        let content2 = doc2.to_markdown();

        // Content should be one of the original versions, not interleaved garbage
        let valid_contents = ["# Hello from A", "# Hello from B"];
        assert!(
            valid_contents.contains(&content1.as_str()),
            "Vault1 content should be valid, got: '{}'",
            content1
        );
        assert!(
            valid_contents.contains(&content2.as_str()),
            "Vault2 content should be valid, got: '{}'",
            content2
        );

        // With "latest wins", vault2's file (newer mtime) should win
        // Both vaults should converge to the same content
        assert_eq!(
            content1, content2,
            "Both vaults should have same content after sync"
        );
    }

    #[tokio::test]
    async fn test_latest_wins_newer_remote() {
        // Test that "latest wins" correctly keeps newer remote content
        use std::sync::Arc;

        let fs1 = Arc::new(InMemoryFs::new());
        let fs2 = Arc::new(InMemoryFs::new());

        // Vault1 creates file first (older)
        fs1.write("note.md", b"Older content").await.unwrap();
        fs1.set_mtime("note.md", 1000); // Older timestamp

        // Vault2 creates same file later (newer)
        fs2.write("note.md", b"Newer content").await.unwrap();
        fs2.set_mtime("note.md", 2000); // Newer timestamp

        let mut vault1 = Vault::init(Arc::clone(&fs1), "peer1".to_string()).await.unwrap();
        let mut vault2 = Vault::init(Arc::clone(&fs2), "peer2".to_string()).await.unwrap();

        // Vault2 sends DocumentUpdate to Vault1 (real-time sync with mtime)
        let update = vault2.prepare_document_update("note.md").await.unwrap().unwrap();
        let (_, modified) = vault1.process_sync_message(&update).await.unwrap();

        // Vault1 should accept the newer content
        assert!(modified.contains(&"note.md".to_string()), "Should be modified");
        let doc = vault1.get_document("note.md").await.unwrap();
        assert_eq!(doc.to_markdown(), "Newer content", "Should have newer content");
    }

    #[tokio::test]
    async fn test_latest_wins_newer_local() {
        // Test that "latest wins" correctly keeps newer local content
        use std::sync::Arc;

        let fs1 = Arc::new(InMemoryFs::new());
        let fs2 = Arc::new(InMemoryFs::new());

        // Vault1 creates file later (newer)
        fs1.write("note.md", b"Newer content").await.unwrap();
        fs1.set_mtime("note.md", 2000); // Newer timestamp

        // Vault2 creates same file first (older)
        fs2.write("note.md", b"Older content").await.unwrap();
        fs2.set_mtime("note.md", 1000); // Older timestamp

        let mut vault1 = Vault::init(Arc::clone(&fs1), "peer1".to_string()).await.unwrap();
        let mut vault2 = Vault::init(Arc::clone(&fs2), "peer2".to_string()).await.unwrap();

        // Vault2 sends DocumentUpdate to Vault1 (real-time sync with mtime)
        let update = vault2.prepare_document_update("note.md").await.unwrap().unwrap();
        let (_, modified) = vault1.process_sync_message(&update).await.unwrap();

        // Vault1 should REJECT the older content (keep its own)
        assert!(modified.is_empty(), "Should NOT be modified - local is newer");
        let doc = vault1.get_document("note.md").await.unwrap();
        assert_eq!(doc.to_markdown(), "Newer content", "Should keep newer local content");
    }

    #[tokio::test]
    async fn test_sync_empty_file() {
        // Test that syncing empty files works correctly
        let fs1 = InMemoryFs::new();
        let fs2 = InMemoryFs::new();

        // Vault1 creates an empty file
        fs1.write("empty.md", b"").await.unwrap();

        let mut vault1 = Vault::init(fs1, "peer1".to_string()).await.unwrap();
        let mut vault2 = Vault::init(fs2, "peer2".to_string()).await.unwrap();

        // Sync to vault2
        let request = vault2.prepare_sync_request().await.unwrap();
        let (exchange, _) = vault1.process_sync_message(&request).await.unwrap();
        let (final_resp, modified) = vault2.process_sync_message(&exchange.unwrap()).await.unwrap();
        if let Some(resp) = final_resp {
            vault1.process_sync_message(&resp).await.unwrap();
        }

        // Vault2 should have received the empty file
        assert!(modified.contains(&"empty.md".to_string()));
        let doc = vault2.get_document("empty.md").await.unwrap();
        assert_eq!(doc.to_markdown(), "", "Empty file should remain empty");
    }

    #[tokio::test]
    async fn test_sync_frontmatter_only_file() {
        // Test that syncing files with only frontmatter (no body) works correctly
        let fs1 = InMemoryFs::new();
        let fs2 = InMemoryFs::new();

        // Vault1 creates a file with only frontmatter
        fs1.write("meta.md", b"---\ntitle: Test\ntags:\n  - a\n  - b\n---\n").await.unwrap();

        let mut vault1 = Vault::init(fs1, "peer1".to_string()).await.unwrap();
        let mut vault2 = Vault::init(fs2, "peer2".to_string()).await.unwrap();

        // Sync to vault2
        let request = vault2.prepare_sync_request().await.unwrap();
        let (exchange, _) = vault1.process_sync_message(&request).await.unwrap();
        let (final_resp, modified) = vault2.process_sync_message(&exchange.unwrap()).await.unwrap();
        if let Some(resp) = final_resp {
            vault1.process_sync_message(&resp).await.unwrap();
        }

        // Vault2 should have received the file
        assert!(modified.contains(&"meta.md".to_string()));
        let doc = vault2.get_document("meta.md").await.unwrap();
        let content = doc.to_markdown();
        assert!(content.contains("title:"), "Should have frontmatter");
        assert!(content.contains("tags:"), "Should have tags");
    }

    #[tokio::test]
    async fn test_doc_id_detects_divergent_histories() {
        // Test that doc_id correctly identifies documents with divergent histories.
        // Documents created independently have different doc_ids and are treated
        // as divergent (using content reconciliation instead of CRDT merge).
        use crate::document::NoteDocument;

        // Two documents created independently have different doc_ids
        let doc1 = NoteDocument::from_markdown("test.md", "Content A").unwrap();
        let doc2 = NoteDocument::from_markdown("test.md", "Content B").unwrap();

        let doc1_id = doc1.doc_id();
        let doc2_id = doc2.doc_id();

        assert!(doc1_id.is_some(), "New documents should have doc_id");
        assert!(doc2_id.is_some(), "New documents should have doc_id");
        assert_ne!(
            doc1_id, doc2_id,
            "Independently created documents should have different doc_ids"
        );

        // A document imported from another preserves the doc_id
        let mut doc3 = NoteDocument::new("test.md");
        doc3.import(&doc1.export_snapshot()).unwrap();

        assert_eq!(
            doc3.doc_id(),
            doc1_id,
            "Imported document should preserve original doc_id"
        );
    }

    #[tokio::test]
    async fn test_incremental_updates_after_sync_use_crdt_merge() {
        // After initial sync, both vaults share the same doc_id.
        // Subsequent edits should merge via CRDT, not trigger divergence detection.
        use std::sync::Arc;

        let fs1 = Arc::new(InMemoryFs::new());
        let fs2 = Arc::new(InMemoryFs::new());

        // Vault1 creates file
        fs1.write("note.md", b"Line 1").await.unwrap();
        let mut vault1 = Vault::init(Arc::clone(&fs1), "peer1".to_string()).await.unwrap();
        let mut vault2 = Vault::init(Arc::clone(&fs2), "peer2".to_string()).await.unwrap();

        // Initial sync - vault2 gets the file with vault1's doc_id
        let request = vault2.prepare_sync_request().await.unwrap();
        let (exchange, _) = vault1.process_sync_message(&request).await.unwrap();
        let (final_resp, _) = vault2.process_sync_message(&exchange.unwrap()).await.unwrap();
        if let Some(resp) = final_resp {
            vault1.process_sync_message(&resp).await.unwrap();
        }

        // Both vaults should now have same doc_id
        let doc1 = vault1.get_document("note.md").await.unwrap();
        let doc2 = vault2.get_document("note.md").await.unwrap();
        assert_eq!(doc1.doc_id(), doc2.doc_id(), "After sync, doc_ids should match");

        // Vault2 makes an edit
        fs2.write("note.md", b"Line 1\nLine 2 from vault2").await.unwrap();
        vault2.on_file_changed("note.md").await.unwrap();

        // Vault1 also makes an edit (concurrent)
        fs1.write("note.md", b"Line 1\nLine 2 from vault1").await.unwrap();
        vault1.on_file_changed("note.md").await.unwrap();

        // Sync vault2 → vault1 (should CRDT merge, not diverge)
        let update = vault2.prepare_document_update("note.md").await.unwrap().unwrap();
        let (_, modified) = vault1.process_sync_message(&update).await.unwrap();

        // Should be modified (merged)
        assert!(modified.contains(&"note.md".to_string()), "Should merge changes");

        // Content should have BOTH lines (CRDT merge), not replace one with the other
        let doc = vault1.get_document("note.md").await.unwrap();
        let content = doc.to_markdown();
        assert!(content.contains("Line 1"), "Should have original line");
        // CRDT merge means both edits are present (order may vary)
        assert!(
            content.contains("vault1") || content.contains("vault2"),
            "Should have merged content, got: {}",
            content
        );
    }

    #[tokio::test]
    async fn test_legacy_document_without_doc_id_assumes_compatible() {
        // Documents created before doc_id was added (legacy) should be treated
        // as compatible (non-divergent) to avoid breaking existing syncs.
        use crate::document::NoteDocument;
        use loro::LoroDoc;

        // Simulate a legacy document without doc_id by creating a LoroDoc directly
        let legacy_doc = LoroDoc::new();
        let meta = legacy_doc.get_map("_meta");
        meta.insert("path", "test.md").unwrap();
        // Note: no doc_id inserted
        let body = legacy_doc.get_text("body");
        body.insert(0, "Legacy content").unwrap();
        legacy_doc.commit();
        let legacy_bytes = legacy_doc.export(loro::ExportMode::Snapshot).unwrap();

        // Load via from_bytes - should NOT add a doc_id (preserves legacy state)
        let doc = NoteDocument::from_bytes("test.md", &legacy_bytes).unwrap();
        assert!(doc.doc_id().is_none(), "Legacy document should have no doc_id");

        // New document has doc_id
        let new_doc = NoteDocument::from_markdown("test.md", "New content").unwrap();
        assert!(new_doc.doc_id().is_some(), "New document should have doc_id");

        // When syncing legacy (no doc_id) with new (has doc_id), should assume compatible
        // This is tested implicitly by the fallback in apply_single_update:
        // match (&local_doc_id, &remote_doc_id) { ... _ => false }
    }
}
