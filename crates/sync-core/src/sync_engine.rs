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
use crate::events::SyncEvent;
use crate::fs::FileSystem;
use crate::sync::{SyncMessage, SyncRequestData, SyncResponseData};
use crate::vault::Vault;

use loro::TreeID;
use std::collections::HashMap;
use thiserror::Error;
use tracing::{debug, info, warn};

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
    pub async fn prepare_sync_request(&self) -> Result<Vec<u8>> {
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

        let bytes =
            bincode::serialize(&msg).map_err(|e| SyncEngineError::Serialization(e.to_string()))?;

        self.emit(SyncEvent::MessageSent {
            message_type: "SyncRequest".into(),
            size: bytes.len(),
            timestamp: self.now_ms(),
        });

        Ok(bytes)
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
        &self,
        data: &[u8],
    ) -> Result<(Option<Vec<u8>>, Vec<String>)> {
        // Ensure consistency before processing any sync message
        self.ensure_consistency().await?;

        self.emit(SyncEvent::MessageReceived {
            message_type: "SyncMessage".into(),
            size: data.len(),
            timestamp: self.now_ms(),
        });

        let msg: SyncMessage = bincode::deserialize(data)
            .map_err(|e| SyncEngineError::Deserialization(e.to_string()))?;

        match msg {
            SyncMessage::SyncRequest {
                registry_version,
                document_versions,
            } => {
                // Peer is requesting sync - respond with SyncExchange (symmetric protocol)
                let exchange = self
                    .prepare_sync_exchange(&registry_version, document_versions)
                    .await?;
                let exchange_bytes = bincode::serialize(&exchange)
                    .map_err(|e| SyncEngineError::Serialization(e.to_string()))?;
                Ok((Some(exchange_bytes), vec![]))
            }

            SyncMessage::SyncResponse {
                registry_updates,
                document_updates,
            } => {
                // Apply registry updates first (handles deletes/renames).
                // The returned deleted_paths filters out document updates that would
                // re-create files the registry just deleted.
                let deleted_paths = if let Some(reg_data) = registry_updates {
                    self.apply_registry_updates(&reg_data).await?
                } else {
                    vec![]
                };
                // Skip document updates for paths deleted in the registry
                let mut document_updates = document_updates;
                for path in &deleted_paths {
                    document_updates.remove(path);
                }
                // Then apply document updates
                let modified = self.apply_document_updates(document_updates).await?;

                // Emit DocumentUpdated for each modified path
                for path in &modified {
                    self.emit(SyncEvent::DocumentUpdated {
                        path: path.clone(),
                        timestamp: self.now_ms(),
                    });
                }

                Ok((None, modified))
            }

            SyncMessage::SyncExchange { response, request } => {
                // Peer responded to our SyncRequest with:
                // - response: updates we need from them
                // - request: their version vectors so we can send them updates

                debug!(
                    "SyncExchange: received {} document updates, {} version vectors",
                    response.document_updates.len(),
                    request.document_versions.len()
                );

                // Track which files we're receiving so we don't echo them back
                let received_files: std::collections::HashSet<String> =
                    response.document_updates.keys().cloned().collect();

                // Apply registry updates first (handles deletes/renames).
                // The returned deleted_paths filters out document updates that would
                // re-create files the registry just deleted.
                let deleted_paths = if let Some(reg_data) = response.registry_updates {
                    self.apply_registry_updates(&reg_data).await?
                } else {
                    vec![]
                };
                // Skip document updates for paths deleted in the registry
                let mut document_updates = response.document_updates;
                for path in &deleted_paths {
                    document_updates.remove(path);
                }

                // Then apply document updates
                let modified = self.apply_document_updates(document_updates).await?;
                debug!(
                    "SyncExchange: modified {} files: {:?}",
                    modified.len(),
                    modified
                );

                // Emit DocumentUpdated for each modified path
                for path in &modified {
                    self.emit(SyncEvent::DocumentUpdated {
                        path: path.clone(),
                        timestamp: self.now_ms(),
                    });
                }

                // Then, prepare updates they need from us (excluding files we just received)
                let our_response = self
                    .prepare_sync_response_data_excluding(
                        &request.registry_version,
                        request.document_versions,
                        &received_files,
                    )
                    .await?;
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

                if modified {
                    self.emit(SyncEvent::DocumentUpdated {
                        path: path.clone(),
                        timestamp: self.now_ms(),
                    });
                }

                Ok((None, if modified { vec![path] } else { vec![] }))
            }

            SyncMessage::FileDeleted { path } => {
                // Handle file deletion via tree operation
                debug!("Received file deletion for: {}", path);

                self.emit(SyncEvent::FileOp {
                    operation: "delete".into(),
                    path: path.clone(),
                    new_path: None,
                    timestamp: self.now_ms(),
                });

                // Mark as synced BEFORE deleting (for echo detection)
                self.mark_synced(&path);
                self.delete_file(&path).await?;
                Ok((None, vec![path]))
            }

            SyncMessage::FileRenamed { old_path, new_path } => {
                // Handle file rename via tree operation
                debug!("Received file rename: {} -> {}", old_path, new_path);

                self.emit(SyncEvent::FileOp {
                    operation: "rename".into(),
                    path: old_path.clone(),
                    new_path: Some(new_path.clone()),
                    timestamp: self.now_ms(),
                });

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
    pub async fn prepare_document_update(&self, path: &str) -> Result<Option<Vec<u8>>> {
        // Ensure document is loaded
        let doc = self.get_document(path).await?;

        // Export a snapshot (for now - could optimize to send incremental updates)
        let snapshot = doc.export_snapshot()?;

        // Get file modification time for "latest wins" conflict resolution
        let mtime = self.fs.stat(path).await.ok().map(|s| s.mtime_millis);

        let msg = SyncMessage::DocumentUpdate {
            path: path.to_string(),
            data: snapshot,
            mtime,
        };

        let bytes =
            bincode::serialize(&msg).map_err(|e| SyncEngineError::Serialization(e.to_string()))?;

        self.emit(SyncEvent::MessageSent {
            message_type: "DocumentUpdate".into(),
            size: bytes.len(),
            timestamp: self.now_ms(),
        });

        Ok(Some(bytes))
    }

    /// Prepare a file deletion message to broadcast.
    pub fn prepare_file_deleted(&self, path: &str) -> Result<Vec<u8>> {
        let msg = SyncMessage::FileDeleted {
            path: path.to_string(),
        };

        let bytes =
            bincode::serialize(&msg).map_err(|e| SyncEngineError::Serialization(e.to_string()))?;

        self.emit(SyncEvent::FileOp {
            operation: "delete".into(),
            path: path.to_string(),
            new_path: None,
            timestamp: self.now_ms(),
        });

        Ok(bytes)
    }

    /// Prepare a file renamed message to broadcast.
    pub fn prepare_file_renamed(&self, old_path: &str, new_path: &str) -> Result<Vec<u8>> {
        let msg = SyncMessage::FileRenamed {
            old_path: old_path.to_string(),
            new_path: new_path.to_string(),
        };

        let bytes =
            bincode::serialize(&msg).map_err(|e| SyncEngineError::Serialization(e.to_string()))?;

        self.emit(SyncEvent::FileOp {
            operation: "rename".into(),
            path: old_path.to_string(),
            new_path: Some(new_path.to_string()),
            timestamp: self.now_ms(),
        });

        Ok(bytes)
    }

    /// Get the registry version vector as bytes.
    fn registry_version(&self) -> Vec<u8> {
        self.registry().state_vv().encode()
    }

    /// Prepare a SyncExchange in response to a SyncRequest.
    ///
    /// This bundles:
    /// - Our response (updates they need from us)
    /// - Our request (our version vectors so they can send us updates)
    async fn prepare_sync_exchange(
        &self,
        their_registry_version: &[u8],
        their_versions: HashMap<String, Vec<u8>>,
    ) -> Result<SyncMessage> {
        // Prepare updates they need from us
        let response = self
            .prepare_sync_response_data(their_registry_version, their_versions)
            .await?;

        // Prepare our version vectors so they can send us updates
        let request = self.prepare_sync_request_data().await?;

        Ok(SyncMessage::SyncExchange { response, request })
    }

    /// Prepare sync request data (our version vectors).
    async fn prepare_sync_request_data(&self) -> Result<SyncRequestData> {
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
        &self,
        their_registry_version: &[u8],
        their_versions: HashMap<String, Vec<u8>>,
    ) -> Result<SyncResponseData> {
        self.prepare_sync_response_data_excluding(
            their_registry_version,
            their_versions,
            &std::collections::HashSet::new(),
        )
        .await
    }

    /// Prepare sync response data, excluding specific files.
    ///
    /// Used when responding to a SyncExchange - we exclude files we just received
    /// to avoid echoing them back. Loro's import creates a local change marker,
    /// so version-based comparison would incorrectly send updates for files
    /// we just imported.
    async fn prepare_sync_response_data_excluding(
        &self,
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
                    let updates = doc.export_updates(&their_version)?;
                    if !updates.is_empty() {
                        document_updates.insert(path, updates);
                    }
                }
            } else {
                // They don't have it - send full snapshot
                document_updates.insert(path, doc.export_snapshot()?);
            }
        }

        // Export registry updates if they have an older version
        let registry_updates = if !their_registry_version.is_empty() {
            if let Ok(their_version) = loro::VersionVector::decode(their_registry_version) {
                match self
                    .registry()
                    .export(loro::ExportMode::updates(&their_version))
                {
                    Ok(updates) if !updates.is_empty() => Some(updates),
                    _ => None,
                }
            } else {
                // Invalid version - send full snapshot
                self.registry().export(loro::ExportMode::snapshot()).ok()
            }
        } else {
            // They don't have registry - send full snapshot
            self.registry().export(loro::ExportMode::snapshot()).ok()
        };

        Ok(SyncResponseData {
            registry_updates,
            document_updates,
        })
    }

    /// Apply registry updates from a sync response.
    ///
    /// Errors here `?`-propagate (whole-batch-fatal): registry-delta corruption means we
    /// can't trust any of the batch, whereas a single corrupt document is per-item-recoverable
    /// — see `apply_document_updates`, which contains per-item to drop the bad one and keep the rest.
    ///
    /// Imports the registry CRDT updates, then cleans up the filesystem for paths the
    /// registry vacated on this device:
    ///
    /// - **Deletes** — a node whose path is now tombstoned in the tree. Detected from the
    ///   pre-rebuild cache because Loro doesn't expose a deleted node's parent (so its path
    ///   isn't walkable after rebuild).
    /// - **Moves** — a node (same TreeID) that now lives at a different path than it did
    ///   before this import (a `tree.mov` on the sender). The old physical .md/.loro would
    ///   otherwise be left stranded as untracked orphans on every receiver. Detected by
    ///   diffing the pre-import `path_to_node` snapshot against the rebuilt cache.
    ///
    /// A vacated path that some alive node NOW occupies is excluded from cleanup (B1): in a
    /// swap (A leaves P1 while B moves into P1 in the same import) deleting B's freshly
    /// arrived file — and dropping its document update — would be permanent data loss.
    ///
    /// Returns the union of removed (deleted + moved-away) paths, so the caller can strip
    /// them from subsequent document updates (which would otherwise re-create the file on
    /// disk under the old path).
    ///
    /// Out of scope here: emitting tombstones for paths that were never cached on this
    /// device (uncached deletes/moves). That's the disk↔registry reconcile work item, not a
    /// gap in this function — this function only reconciles paths it can observe vacating.
    ///
    /// Duplicate-node shadowing gap: when two alive nodes share one path (known production
    /// debris), the rebuilt cache keeps one winner per path, so `node_to_new_path` (built by
    /// inverting that cache) omits the shadowed node's TreeID. A genuine move of the shadowed
    /// node is therefore not detected, leaving its old .md/.loro stranded. The failure mode is
    /// always a missed cleanup (an orphan), never a false deletion. Resolved by the queued
    /// registry-dedupe work item that removes duplicate nodes.
    async fn apply_registry_updates(&self, data: &[u8]) -> Result<Vec<String>> {
        debug!("apply_registry_updates: data_len={}", data.len());

        // Snapshot the pre-import path→node mapping so we can detect moves after the cache is
        // rebuilt. TreeID is Copy; `.clone()` copies the HashMap out of the guard, which is a
        // temporary dropped at the end of this statement — before rebuild_path_cache
        // re-acquires the same lock.
        let pre_import_paths: HashMap<String, TreeID> = self.path_to_node().clone();

        // Import registry updates
        self.registry_mut().import(data).map_err(|e| {
            SyncEngineError::Deserialization(format!("Registry import failed: {}", e))
        })?;

        // Collect deleted paths from the cache BEFORE rebuilding it.
        //
        // After import, the Loro tree marks deleted nodes internally, but
        // `get_node_path` returns None for deleted nodes because Loro doesn't
        // expose parent links for them. The path_to_node cache still has the
        // pre-deletion mapping, so we check each cached path against the tree
        // before the cache is cleared by rebuild_path_cache().
        let deleted_paths: Vec<String> = {
            let tree = self.file_tree();
            self.path_to_node()
                .iter()
                .filter(|(_, node_id)| tree.is_node_deleted(node_id).unwrap_or(false))
                .map(|(path, _)| path.clone())
                .collect()
        };

        // Rebuild path cache from the updated tree. This also re-derives the deleted-paths
        // guard set from registry truth (reading each deleted file node's `path` meta) and
        // applies "alive wins", so a peer's legitimate re-create at a previously-deleted
        // path is not blocked — no separate record/clear bookkeeping is needed here.
        self.rebuild_path_cache();

        // Alive-wins for the captured deleted set (the analogue of the move B1 exclusion
        // below): deleted_paths was built from the PRE-import cache, which — under a
        // duplicate-node pair, two alive twins at one path — may have held the now-tombstoned
        // twin. rebuild_path_cache's alive-wins only repairs the registry guard set, not this
        // already-captured local vec, so a path an alive twin still occupies must be dropped
        // here or apply_registry_changes would physically delete a live file (Log.md /
        // Working Memory.md data loss).
        let deleted_paths: Vec<String> = {
            let rebuilt_cache = self.path_to_node();
            deleted_paths
                .into_iter()
                .filter(|p| !rebuilt_cache.contains_key(p.as_str()))
                .collect()
        };

        // Detect moves: an old path whose node (same TreeID) now lives at a different path.
        // This is orthogonal to the deleted-paths set above (which keys on tombstoned nodes);
        // a moved node stays alive, just under a new path.
        let mut removed_paths = deleted_paths;
        {
            let rebuilt_cache = self.path_to_node();
            let node_to_new_path: HashMap<TreeID, &String> =
                rebuilt_cache.iter().map(|(p, id)| (*id, p)).collect();

            for (old_path, node_id) in &pre_import_paths {
                if let Some(new_path) = node_to_new_path.get(node_id)
                    && *new_path != old_path
                {
                    // B1 exclusion: a vacated path that an alive node now occupies must be
                    // neither fs-cleaned nor filtered from doc updates (the swap case).
                    if !rebuilt_cache.contains_key(old_path) {
                        removed_paths.push(old_path.clone());
                    }
                }
            }
        }

        // Clean up filesystem for vacated (deleted + moved-away) paths
        self.apply_registry_changes(&removed_paths).await?;

        // Save updated registry to disk
        let registry_bytes = self
            .registry()
            .export(loro::ExportMode::snapshot())
            .map_err(|e| {
                crate::vault::VaultError::Other(format!("Registry export failed: {}", e))
            })?;
        self.fs
            .write(
                &format!("{}/registry.loro", crate::vault::SYNC_DIR),
                &registry_bytes,
            )
            .await
            .map_err(crate::vault::VaultError::from)?;

        // Mark registry as synced so it will be reconciled before next sync import
        self.mark_registry_synced();

        debug!(
            "apply_registry_updates: complete, removed={:?}",
            removed_paths
        );
        Ok(removed_paths)
    }

    /// Apply filesystem cleanup for a set of deleted paths.
    ///
    /// Removes the .md file, .loro document, and document cache entry for each
    /// path. Takes an explicit list rather than re-iterating the tree because
    /// Loro doesn't expose parent links for deleted nodes, making path resolution
    /// unreliable after deletion.
    async fn apply_registry_changes(&self, deleted_paths: &[String]) -> Result<()> {
        for path in deleted_paths {
            // Remove from filesystem
            if self.fs.exists(path).await.unwrap_or(false) {
                debug!("apply_registry_changes: deleting {}", path);
                // Mark as synced BEFORE deleting (for echo detection)
                self.mark_synced(path);
                if let Err(e) = self.fs.delete(path).await {
                    warn!("Failed to delete {}: {}", path, e);
                }
            }

            // Remove .loro document
            let sync_path = self.document_sync_path(path);
            if self.fs.exists(&sync_path).await.unwrap_or(false) {
                if let Err(e) = self.fs.delete(&sync_path).await {
                    warn!("Failed to delete .loro file {}: {}", sync_path, e);
                }
            }

            // Remove from documents cache
            self.documents_mut().remove(path);
        }

        Ok(())
    }

    /// Apply document updates from a sync response.
    ///
    /// Note: SyncResponse doesn't include mtime, so "latest wins" falls back to "remote wins"
    /// for initial sync. Real-time DocumentUpdate messages include mtime for proper resolution.
    async fn apply_document_updates(
        &self,
        updates: HashMap<String, Vec<u8>>,
    ) -> Result<Vec<String>> {
        let mut modified = Vec::new();

        for (path, data) in updates {
            // No mtime available in bulk sync - uses "remote wins" for divergent histories.
            //
            // Contain per-document failures: one corrupt entry must not abort the
            // whole batch and drop every other (valid) document with it. There is no
            // per-item retry path, so a partial set of applied paths is the correct
            // outcome - the caller emits events only for the documents that landed.
            match self.apply_single_update(&path, &data, None).await {
                Ok(true) => modified.push(path),
                Ok(false) => {}
                Err(e) => warn!("apply_document_updates: skipping {}: {}", path, e),
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
        &self,
        path: &str,
        data: &[u8],
        remote_mtime: Option<u64>,
    ) -> Result<bool> {
        debug!("apply_single_update: {} - data_len={}", path, data.len());

        // Check if document exists (in cache or on disk)
        let sync_path = self.document_sync_path(path);
        let exists_in_cache = self.documents().contains_key(path);
        let exists_on_disk = self
            .fs
            .exists(&sync_path)
            .await
            .map_err(crate::vault::VaultError::from)?;

        if exists_in_cache || exists_on_disk {
            // Get local mtime and device author before borrowing doc (needed for "latest wins" comparison)
            let local_mtime = self.fs.stat(path).await.ok().map(|s| s.mtime_millis);
            let author = self.loro_author;

            // Note: Staleness reconciliation is handled by ensure_consistency() at the
            // start of process_sync_message(). Documents are guaranteed to be consistent
            // with the filesystem before this point.

            // Document exists - check for divergent histories before merging
            let mut doc = self.get_document_mut(path).await?;
            let local_vv = doc.version();

            // Create temp doc FROM LOCAL STATE, then import remote to get merged version
            // This correctly handles incremental updates (not just full snapshots)
            let mut temp_doc = NoteDocument::from_bytes(path, &doc.export_snapshot()?, author)?;
            temp_doc.import(data)?;
            let merged_vv = temp_doc.version();

            // Check if the merge caused any change
            let local_includes_merged = local_vv.includes_vv(&merged_vv);

            // Check if histories are truly divergent by comparing doc_ids.
            // Documents from the same source (synced) share the same doc_id.
            // Documents created independently have different doc_ids.
            let remote_only_doc = NoteDocument::from_bytes(path, data, author)?;

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
                // Update the document in cache before saving
                self.update_document(path, doc);
                // Mark as synced BEFORE writing to disk (for echo detection)
                self.mark_synced(path);
                self.save_document(path).await?;
                debug!("apply_single_update: saved {} to disk", path);
            }

            Ok(modified)
        } else {
            // Before creating a new document, check whether this path's registry tree node
            // is currently deleted. A deleted path means the local device explicitly deleted
            // this file (or received the deletion via registry sync). Creating it here would
            // resurrect it, causing ping-pong deletion loops between peers.
            //
            // The deleted-paths set is registry-truth, derived in rebuild_path_cache from the
            // persisted tree, so it guards across a daemon restart (unlike the old in-memory
            // session set). We only skip when the path is KNOWN deleted; brand-new paths are
            // not in the set and still create correctly.
            //
            // Legit re-create: when a peer creates a brand-new registry node at a
            // previously-deleted path, apply_registry_updates runs rebuild_path_cache first,
            // which sees the path as alive and drops it from the set ("alive wins"). The next
            // DocumentUpdate for that path reaches here not-deleted and the create proceeds.
            // Locally, register_file makes the path alive in the cache for the same effect.
            if self.is_path_deleted_in_registry(path) {
                info!(
                    "apply_single_update: skipping create for registry-deleted path: {}",
                    path
                );
                return Ok(false);
            }

            // Document is new - create directly from sync data; new ops author under this device
            let doc = NoteDocument::from_bytes(path, data, self.loro_author)?;

            // Mark as synced BEFORE writing to disk (for echo detection)
            self.mark_synced(path);

            // Save to disk
            let snapshot = doc.export_snapshot()?;
            self.fs
                .atomic_write(&sync_path, &snapshot)
                .await
                .map_err(crate::vault::VaultError::from)?;
            self.fs
                .write(path, doc.to_markdown().as_bytes())
                .await
                .map_err(crate::vault::VaultError::from)?;

            // Note: Don't register in tree here - tree sync handles that via registry.
            // Registering here would create duplicate nodes with different IDs.

            // Add to cache
            self.documents_mut().insert(path.to_string(), doc);

            debug!("apply_single_update: created new {} from sync data", path);
            Ok(true)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::PeerId;
    use crate::fs::InMemoryFs;

    fn test_author() -> PeerId {
        PeerId::from_bytes([1u8; 32])
    }

    fn test_author_2() -> PeerId {
        PeerId::from_bytes([2u8; 32])
    }

    /// Build two in-memory vaults, seeding each with `(path, content)` files
    /// before `Vault::init` indexes them. Vault A is authored by `test_author`,
    /// vault B by `test_author_2` — the same construction the hand-rolled tests
    /// use. For tests that add files after init (via `on_file_changed`) or need a
    /// retained `Arc<InMemoryFs>` handle, construct manually and drive the
    /// handshake with [`full_sync`].
    async fn two_vaults(
        files_a: &[(&str, &str)],
        files_b: &[(&str, &str)],
    ) -> (Vault<InMemoryFs>, Vault<InMemoryFs>) {
        let fs_a = InMemoryFs::new();
        let fs_b = InMemoryFs::new();

        for (path, content) in files_a {
            fs_a.write(path, content.as_bytes()).await.unwrap();
        }
        for (path, content) in files_b {
            fs_b.write(path, content.as_bytes()).await.unwrap();
        }

        let vault_a = Vault::init(fs_a, test_author()).await.unwrap();
        let vault_b = Vault::init(fs_b, test_author_2()).await.unwrap();
        (vault_a, vault_b)
    }

    /// Drive the complete three-message sync handshake with `a` as the initiator
    /// and return `(modified_a, modified_b)` — the paths each vault received.
    ///
    /// The exchange is: A sends a SyncRequest → B answers with a SyncExchange
    /// (its response plus its own request) → A applies it and sends back a final
    /// SyncResponse → B applies that and the handshake terminates. The protocol
    /// always produces a final SyncResponse, so the `.unwrap()` chain is exact;
    /// getting this order wrong is the latent "pass-by-luck" risk this helper
    /// removes. Tests that assert on the intermediate messages keep the manual
    /// form.
    async fn full_sync<F: crate::fs::FileSystem>(
        a: &Vault<F>,
        b: &Vault<F>,
    ) -> (Vec<String>, Vec<String>) {
        let request = a.prepare_sync_request().await.unwrap();
        let (exchange, _) = b.process_sync_message(&request).await.unwrap();
        let (final_response, modified_a) =
            a.process_sync_message(&exchange.unwrap()).await.unwrap();
        let (_, modified_b) = b
            .process_sync_message(&final_response.unwrap())
            .await
            .unwrap();
        (modified_a, modified_b)
    }

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
        let vault1 = Vault::init(fs1, test_author()).await.unwrap();
        let vault2 = Vault::init(fs2, test_author_2()).await.unwrap();

        // Vault 1 sends sync request to Vault 2
        let request = vault1.prepare_sync_request().await.unwrap();

        // Vault 2 processes request and sends SyncExchange (response + its own request)
        let (exchange, _) = vault2.process_sync_message(&request).await.unwrap();
        assert!(exchange.is_some(), "Should return SyncExchange");

        // Vault 1 processes the exchange:
        // - Applies file2 from vault2
        // - Sends back SyncResponse with file1 for vault2
        let (final_response, modified1) = vault1
            .process_sync_message(&exchange.unwrap())
            .await
            .unwrap();
        assert!(final_response.is_some(), "Should return final SyncResponse");
        assert!(
            modified1.contains(&"file2.md".to_string()),
            "Vault1 should receive file2"
        );

        // Vault 2 processes the final response
        let (none, modified2) = vault2
            .process_sync_message(&final_response.unwrap())
            .await
            .unwrap();
        assert!(none.is_none(), "No more messages needed");
        assert!(
            modified2.contains(&"file1.md".to_string()),
            "Vault2 should receive file1"
        );

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

        let vault1 = Vault::init(fs1, test_author()).await.unwrap();
        let vault2 = Vault::init(fs2, test_author_2()).await.unwrap();

        // Empty vault sends sync request
        let request = vault2.prepare_sync_request().await.unwrap();

        // Vault 1 responds with SyncExchange
        let (exchange, _) = vault1.process_sync_message(&request).await.unwrap();

        // Vault 2 processes exchange - should receive both files
        let (final_response, modified) = vault2
            .process_sync_message(&exchange.unwrap())
            .await
            .unwrap();

        assert!(modified.contains(&"note1.md".to_string()));
        assert!(modified.contains(&"note2.md".to_string()));

        // Final response exists (vault2 sends SyncResponse even if empty)
        assert!(final_response.is_some());

        // Vault 1 processes final response - nothing new (vault2 was empty)
        let (none, modified1) = vault1
            .process_sync_message(&final_response.unwrap())
            .await
            .unwrap();
        assert!(none.is_none(), "No more messages after SyncResponse");
        assert!(modified1.is_empty(), "Vault1 already had everything");
    }

    /// A corrupt document in a sync batch must not take down the whole batch.
    /// Before the per-item containment fix, `apply_document_updates` propagated
    /// the first error and dropped every other (valid) document with it.
    #[tokio::test]
    async fn test_apply_document_updates_continues_on_corrupt_entry() {
        use std::sync::Arc;

        let fs = Arc::new(InMemoryFs::new());
        let vault = Vault::init(Arc::clone(&fs), test_author()).await.unwrap();

        // A real snapshot for a path the receiver doesn't have yet -> lands as a
        // new document.
        let valid_doc =
            NoteDocument::from_markdown("good.md", "# Good", test_author_2()).unwrap();
        let valid_snapshot = valid_doc.export_snapshot().unwrap();

        let mut document_updates = HashMap::new();
        document_updates.insert("good.md".to_string(), valid_snapshot);
        document_updates.insert("bad.md".to_string(), vec![0xDE, 0xAD, 0xBE, 0xEF]);

        let response = SyncMessage::SyncResponse {
            registry_updates: None,
            document_updates,
        };
        let response_bytes = bincode::serialize(&response).unwrap();

        // The corrupt entry must be skipped, not abort the batch.
        let (followup, modified) = vault.process_sync_message(&response_bytes).await.unwrap();
        assert!(followup.is_none(), "SyncResponse needs no follow-up message");

        assert_eq!(
            modified.len(),
            1,
            "only one document should be reported applied"
        );
        assert!(
            modified.contains(&"good.md".to_string()),
            "good.md must be in the applied list"
        );

        // The valid document landed and is readable.
        let good = vault.get_document("good.md").await.unwrap();
        assert!(good.to_markdown().contains("Good"));
        assert!(
            fs.exists("good.md").await.unwrap(),
            "valid document markdown should be written to disk"
        );

        // The corrupt document was never written to disk. (get_document can't be
        // used as the check here: it falls back to a fresh empty doc for unknown
        // paths, so it always returns Ok.)
        let bad_sync_path = vault.document_sync_path("bad.md");
        assert!(
            !fs.exists(&bad_sync_path).await.unwrap(),
            "corrupt document must not be applied to disk"
        );
        assert!(
            !fs.exists("bad.md").await.unwrap(),
            "corrupt document markdown must not be written"
        );
    }

    #[tokio::test]
    async fn test_document_update_broadcast() {
        let fs1 = InMemoryFs::new();
        let fs2 = InMemoryFs::new();

        let vault1 = Vault::init(fs1, test_author()).await.unwrap();
        let vault2 = Vault::init(fs2, test_author_2()).await.unwrap();

        // Create and sync initial content
        vault1
            .fs
            .write("note.md", b"Initial content")
            .await
            .unwrap();
        vault1.on_file_changed("note.md").await.unwrap();

        // Full sync to get vault2 up to date
        full_sync(&vault2, &vault1).await;

        // Now vault1 makes a change
        vault1
            .fs
            .write("note.md", b"Updated content")
            .await
            .unwrap();
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
        let doc1 = NoteDocument::from_markdown("test.md", "# Hello", test_author()).unwrap();
        let v1 = doc1.version().encode();

        // Create another document and import doc1's state
        let mut doc2 = NoteDocument::new("test.md", test_author_2());
        doc2.import(&doc1.export_snapshot().unwrap()).unwrap();
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

        let vault1 = Vault::init(fs1, test_author()).await.unwrap();
        let vault2 = Vault::init(fs2, test_author_2()).await.unwrap();

        // Vault1 creates a file
        vault1.fs.write("note.md", b"# Original").await.unwrap();
        vault1.on_file_changed("note.md").await.unwrap();

        // Sync to vault2
        let request = vault2.prepare_sync_request().await.unwrap();
        let (exchange, _) = vault1.process_sync_message(&request).await.unwrap();
        let (_, modified) = vault2
            .process_sync_message(&exchange.unwrap())
            .await
            .unwrap();

        // Vault2 should have received the file
        assert!(modified.contains(&"note.md".to_string()));

        // Verify content matches
        let doc1 = vault1.get_document("note.md").await.unwrap();
        let doc2 = vault2.get_document("note.md").await.unwrap();
        assert_eq!(doc1.to_markdown(), doc2.to_markdown());

        // Apply the same sync again - should be a no-op
        let request2 = vault2.prepare_sync_request().await.unwrap();
        let (exchange2, _) = vault1.process_sync_message(&request2).await.unwrap();
        let (_, modified2) = vault2
            .process_sync_message(&exchange2.unwrap())
            .await
            .unwrap();

        // Nothing should be modified (already in sync)
        assert!(modified2.is_empty(), "Re-sync should not modify anything");
    }

    #[tokio::test]
    async fn test_document_update_is_idempotent() {
        // Test that receiving the same DocumentUpdate twice doesn't cause issues
        let fs1 = InMemoryFs::new();
        let fs2 = InMemoryFs::new();

        let vault1 = Vault::init(fs1, test_author()).await.unwrap();
        let vault2 = Vault::init(fs2, test_author_2()).await.unwrap();

        // Vault1 creates a file
        vault1.fs.write("note.md", b"# Content").await.unwrap();
        vault1.on_file_changed("note.md").await.unwrap();

        // Get the document update
        let update = vault1
            .prepare_document_update("note.md")
            .await
            .unwrap()
            .unwrap();

        // Apply to vault2 first time
        let (_, modified1) = vault2.process_sync_message(&update).await.unwrap();
        assert!(
            modified1.contains(&"note.md".to_string()),
            "First apply should modify"
        );

        // Apply the same update again
        let (_, modified2) = vault2.process_sync_message(&update).await.unwrap();
        assert!(
            modified2.is_empty(),
            "Second apply should be no-op (idempotent)"
        );

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

        let vault1 = Vault::init(fs1, test_author()).await.unwrap();
        let vault2 = Vault::init(fs2, test_author_2()).await.unwrap();

        // Vault1 creates a file with specific content
        let content = "Hello";
        vault1
            .fs
            .write("note.md", content.as_bytes())
            .await
            .unwrap();
        vault1.on_file_changed("note.md").await.unwrap();

        // Sync vault1 → vault2
        full_sync(&vault2, &vault1).await;

        // Simulate file watcher: vault2 calls on_file_changed after sync writes to disk.
        // This is the bug scenario - previously created new peer ID and duplicated content.
        vault2.on_file_changed("note.md").await.unwrap();

        // Sync vault2 → vault1 (this would cause duplication before the fix)
        full_sync(&vault1, &vault2).await;

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

        let vault1 = Vault::init(fs1, test_author()).await.unwrap();
        let vault2 = Vault::init(fs2, test_author_2()).await.unwrap();

        // Vault1 creates initial content
        vault1.fs.write("note.md", b"Hello").await.unwrap();
        vault1.on_file_changed("note.md").await.unwrap();

        // Sync to vault2
        full_sync(&vault2, &vault1).await;

        // Vault2 makes a local edit
        vault2.fs.write("note.md", b"Hello World").await.unwrap();
        vault2.on_file_changed("note.md").await.unwrap();

        // Sync back to vault1
        full_sync(&vault1, &vault2).await;

        // Vault1 should have the updated content
        let doc = vault1.get_document("note.md").await.unwrap();
        assert_eq!(
            doc.to_markdown(),
            "Hello World",
            "Edit should propagate correctly"
        );
    }

    #[tokio::test]
    async fn test_diff_merge_preserves_peer_id() {
        // Test that diff-and-merge updates don't create new peer IDs
        let fs = InMemoryFs::new();
        let vault = Vault::init(fs, test_author()).await.unwrap();

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
        let vault1 = Vault::init(Arc::clone(&fs1), test_author()).await.unwrap();

        // Sync to vault2
        fs2.mkdir(".sync").await.unwrap();
        fs2.mkdir(".sync/documents").await.unwrap();
        let vault2 = Vault::init(Arc::clone(&fs2), test_author_2())
            .await
            .unwrap();

        full_sync(&vault2, &vault1).await;

        // Simulate external modification on vault2 (plugin was off)
        fs2.write("note.md", b"Modified externally").await.unwrap();

        // Reload vault2 - this triggers reconcile() -> reindex_file()
        let vault2_reloaded = Vault::load(Arc::clone(&fs2), test_author_2())
            .await
            .unwrap();

        // Sync back to vault1
        full_sync(&vault1, &vault2_reloaded).await;

        // Verify content is NOT duplicated
        let doc = vault1.get_document("note.md").await.unwrap();
        let content = doc.to_markdown();
        assert_eq!(
            content, "Modified externally",
            "Content should not be duplicated after reconcile"
        );
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
        let vault1 = Vault::init(Arc::clone(&fs1), test_author()).await.unwrap();

        // Sync to vault2
        let vault2 = Vault::init(Arc::clone(&fs2), test_author_2())
            .await
            .unwrap();
        full_sync(&vault2, &vault1).await;

        // Clear vault2's in-memory cache (simulate cold cache)
        vault2.documents_mut().clear();

        // Make an edit and call on_file_changed (the .loro exists on disk but not in cache)
        fs2.write("note.md", b"Hello World").await.unwrap();
        vault2.on_file_changed("note.md").await.unwrap();

        // Sync back to vault1
        full_sync(&vault1, &vault2).await;

        // Verify content is correct (not duplicated)
        let doc = vault1.get_document("note.md").await.unwrap();
        let content = doc.to_markdown();
        assert_eq!(
            content, "Hello World",
            "Cold cache should not cause duplication"
        );
    }

    #[tokio::test]
    async fn test_file_migration_preserves_peer_id() {
        // Test that file migration during reconcile preserves peer ID
        use std::sync::Arc;

        let fs1 = Arc::new(InMemoryFs::new());
        let fs2 = Arc::new(InMemoryFs::new());

        // Initialize vault1 with a file
        fs1.write("old_name.md", b"Content ABC").await.unwrap();
        let vault1 = Vault::init(Arc::clone(&fs1), test_author()).await.unwrap();

        // Get the peer ID count from the original document
        let doc1 = vault1.get_document("old_name.md").await.unwrap();
        let original_peer_count = doc1.version().len();

        // Sync to vault2
        let vault2 = Vault::init(Arc::clone(&fs2), test_author_2())
            .await
            .unwrap();
        full_sync(&vault2, &vault1).await;

        // Simulate file rename on vault2 (plugin was off)
        let content = fs2.read("old_name.md").await.unwrap();
        fs2.write("new_name.md", &content).await.unwrap();
        fs2.delete("old_name.md").await.unwrap();

        // Reload vault2 - this triggers reconcile() -> migrate_document()
        let vault2_reloaded = Vault::load(Arc::clone(&fs2), test_author_2())
            .await
            .unwrap();

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
        let vault1 = Vault::init(Arc::clone(&fs1), test_author()).await.unwrap();
        let vault2 = Vault::init(Arc::clone(&fs2), test_author_2())
            .await
            .unwrap();

        // Sync the two vaults
        full_sync(&vault2, &vault1).await;

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

        let vault1 = Vault::init(Arc::clone(&fs1), test_author()).await.unwrap();
        let vault2 = Vault::init(Arc::clone(&fs2), test_author_2())
            .await
            .unwrap();

        // Vault2 sends DocumentUpdate to Vault1 (real-time sync with mtime)
        let update = vault2
            .prepare_document_update("note.md")
            .await
            .unwrap()
            .unwrap();
        let (_, modified) = vault1.process_sync_message(&update).await.unwrap();

        // Vault1 should accept the newer content
        assert!(
            modified.contains(&"note.md".to_string()),
            "Should be modified"
        );
        let doc = vault1.get_document("note.md").await.unwrap();
        assert_eq!(
            doc.to_markdown(),
            "Newer content",
            "Should have newer content"
        );
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

        let vault1 = Vault::init(Arc::clone(&fs1), test_author()).await.unwrap();
        let vault2 = Vault::init(Arc::clone(&fs2), test_author_2())
            .await
            .unwrap();

        // Vault2 sends DocumentUpdate to Vault1 (real-time sync with mtime)
        let update = vault2
            .prepare_document_update("note.md")
            .await
            .unwrap()
            .unwrap();
        let (_, modified) = vault1.process_sync_message(&update).await.unwrap();

        // Vault1 should REJECT the older content (keep its own)
        assert!(
            modified.is_empty(),
            "Should NOT be modified - local is newer"
        );
        let doc = vault1.get_document("note.md").await.unwrap();
        assert_eq!(
            doc.to_markdown(),
            "Newer content",
            "Should keep newer local content"
        );
    }

    #[tokio::test]
    async fn test_sync_empty_file() {
        // Test that syncing empty files works correctly
        // Vault1 creates an empty file
        let (vault1, vault2) = two_vaults(&[("empty.md", "")], &[]).await;

        // Sync to vault2
        let (modified, _) = full_sync(&vault2, &vault1).await;

        // Vault2 should have received the empty file
        assert!(modified.contains(&"empty.md".to_string()));
        let doc = vault2.get_document("empty.md").await.unwrap();
        assert_eq!(doc.to_markdown(), "", "Empty file should remain empty");
    }

    #[tokio::test]
    async fn test_sync_frontmatter_only_file() {
        // Test that syncing files with only frontmatter (no body) works correctly
        // Vault1 creates a file with only frontmatter
        let (vault1, vault2) =
            two_vaults(&[("meta.md", "---\ntitle: Test\ntags:\n  - a\n  - b\n---\n")], &[]).await;

        // Sync to vault2
        let (modified, _) = full_sync(&vault2, &vault1).await;

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
        let doc1 = NoteDocument::from_markdown("test.md", "Content A", test_author()).unwrap();
        let doc2 = NoteDocument::from_markdown("test.md", "Content B", test_author_2()).unwrap();

        let doc1_id = doc1.doc_id();
        let doc2_id = doc2.doc_id();

        assert!(doc1_id.is_some(), "New documents should have doc_id");
        assert!(doc2_id.is_some(), "New documents should have doc_id");
        assert_ne!(
            doc1_id, doc2_id,
            "Independently created documents should have different doc_ids"
        );

        // A document imported from another preserves the doc_id
        // Use different peer_id to avoid Loro merge conflicts with same-peer operations
        let mut doc3 = NoteDocument::new("test.md", test_author_2());
        doc3.import(&doc1.export_snapshot().unwrap()).unwrap();

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
        let vault1 = Vault::init(Arc::clone(&fs1), test_author()).await.unwrap();
        let vault2 = Vault::init(Arc::clone(&fs2), test_author_2())
            .await
            .unwrap();

        // Initial sync - vault2 gets the file with vault1's doc_id
        full_sync(&vault2, &vault1).await;

        // Both vaults should now have same doc_id
        let doc1 = vault1.get_document("note.md").await.unwrap();
        let doc2 = vault2.get_document("note.md").await.unwrap();
        assert_eq!(
            doc1.doc_id(),
            doc2.doc_id(),
            "After sync, doc_ids should match"
        );

        // Vault2 makes an edit
        fs2.write("note.md", b"Line 1\nLine 2 from vault2")
            .await
            .unwrap();
        vault2.on_file_changed("note.md").await.unwrap();

        // Vault1 also makes an edit (concurrent)
        fs1.write("note.md", b"Line 1\nLine 2 from vault1")
            .await
            .unwrap();
        vault1.on_file_changed("note.md").await.unwrap();

        // Sync vault2 → vault1 (should CRDT merge, not diverge)
        let update = vault2
            .prepare_document_update("note.md")
            .await
            .unwrap()
            .unwrap();
        let (_, modified) = vault1.process_sync_message(&update).await.unwrap();

        // Should be modified (merged)
        assert!(
            modified.contains(&"note.md".to_string()),
            "Should merge changes"
        );

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
        let doc = NoteDocument::from_bytes("test.md", &legacy_bytes, test_author()).unwrap();
        assert!(
            doc.doc_id().is_none(),
            "Legacy document should have no doc_id"
        );

        // New document has doc_id
        let new_doc = NoteDocument::from_markdown("test.md", "New content", test_author()).unwrap();
        assert!(
            new_doc.doc_id().is_some(),
            "New document should have doc_id"
        );

        // When syncing legacy (no doc_id) with new (has doc_id), should assume compatible
        // This is tested implicitly by the fallback in apply_single_update:
        // match (&local_doc_id, &remote_doc_id) { ... _ => false }
    }

    // ========== Resurrection Guard Tests ==========

    /// A locally-deleted file must not be resurrected by an inbound DocumentUpdate for
    /// the same path. Without the fix, apply_single_update's new-document branch would
    /// create the file unconditionally because neither cache nor disk have it.
    #[tokio::test]
    async fn test_document_update_skipped_for_registry_deleted_path() {
        let fs1 = InMemoryFs::new();
        let fs2 = InMemoryFs::new();

        // Vault 1 creates note.md and syncs it to vault 2.
        fs1.write("note.md", b"# Hello").await.unwrap();
        let vault1 = Vault::init(fs1, test_author()).await.unwrap();
        vault1.on_file_changed("note.md").await.unwrap();
        let vault2 = Vault::init(fs2, test_author_2()).await.unwrap();

        full_sync(&vault2, &vault1).await;
        assert!(vault2.fs.exists("note.md").await.unwrap(), "setup: vault2 should have note.md");

        // Vault 2 deletes the file locally: remove from disk first (user action),
        // then update the registry tree via delete_file.
        vault2.fs.delete("note.md").await.unwrap();
        vault2.delete_file("note.md").await.unwrap();
        assert!(!vault2.fs.exists("note.md").await.unwrap(), "note.md should be gone after delete");
        assert!(vault2.is_file_deleted("note.md"), "registry should show path as deleted");

        // Vault 1 still has the file and broadcasts a DocumentUpdate (real-time sync path).
        let update = vault1
            .prepare_document_update("note.md")
            .await
            .unwrap()
            .unwrap();

        // Vault 2 receives the DocumentUpdate — must NOT resurrect the file.
        let (_, modified) = vault2.process_sync_message(&update).await.unwrap();

        assert!(
            modified.is_empty(),
            "DocumentUpdate for a locally-deleted path must be skipped (got modified={:?})",
            modified
        );
        assert!(
            !vault2.fs.exists("note.md").await.unwrap(),
            "deleted file must not reappear on disk after inbound DocumentUpdate"
        );
    }

    /// Both peers delete a file; then one peer creates a brand-new file at the same path
    /// (a fresh registry node). The other peer must receive it — the tombstone from the
    /// earlier deletion must be cleared when the new alive registry node arrives.
    ///
    /// Without alive-wins in rebuild_path_cache, vault2's deleted-paths set blocks the
    /// DocumentUpdate forever and note.md never reappears on vault2's filesystem.
    #[tokio::test]
    async fn test_legit_recreate_after_registry_alive_node_applies() {
        let fs1 = InMemoryFs::new();
        let fs2 = InMemoryFs::new();

        // Both vaults start with note.md and sync it.
        fs1.write("note.md", b"# Original").await.unwrap();
        let vault1 = Vault::init(fs1, test_author()).await.unwrap();
        vault1.on_file_changed("note.md").await.unwrap();
        let vault2 = Vault::init(fs2, test_author_2()).await.unwrap();

        full_sync(&vault2, &vault1).await;
        assert!(vault2.fs.exists("note.md").await.unwrap(), "setup: both vaults should have note.md");

        // Both vaults delete note.md independently — both get tombstones.
        vault1.fs.delete("note.md").await.unwrap();
        vault1.delete_file("note.md").await.unwrap();
        vault2.fs.delete("note.md").await.unwrap();
        vault2.delete_file("note.md").await.unwrap();
        assert!(vault1.is_path_deleted_in_registry("note.md"), "vault1 must mark the path deleted");
        assert!(vault2.is_path_deleted_in_registry("note.md"), "vault2 must mark the path deleted");

        // Vault 1 creates a brand-new note.md (new registry node at the same path).
        vault1.fs.write("note.md", b"# Brand new").await.unwrap();
        vault1.on_file_changed("note.md").await.unwrap();
        // on_file_changed -> register_file writes a new alive node for the path, so the
        // registry no longer reports it deleted. This intentionally checks registry-tree
        // truth (is_file_deleted), NOT the deleted_paths session set: by design that set
        // keeps a stale entry until the next rebuild_path_cache, and the entry is
        // unreachable for in-cache documents (inbound updates take the exists/merge branch).
        assert!(!vault1.is_file_deleted("note.md"), "register_file must make the path alive again");

        // Vault 1 syncs to vault 2. The SyncExchange delivers:
        // 1. Registry updates — vault1's new alive node for note.md. apply_registry_updates
        //    runs rebuild_path_cache, whose alive-wins drop clears the path from vault2's
        //    deleted-paths set.
        // 2. Document updates — vault2 can now create note.md from the sync data.
        let request2 = vault2.prepare_sync_request().await.unwrap();
        let (exchange2, _) = vault1.process_sync_message(&request2).await.unwrap();
        let (_, modified) = vault2
            .process_sync_message(&exchange2.unwrap())
            .await
            .unwrap();

        assert!(
            modified.contains(&"note.md".to_string()),
            "legit re-create must propagate to vault2 (got modified={:?})",
            modified
        );
        assert!(
            vault2.fs.exists("note.md").await.unwrap(),
            "re-created file must appear on vault2's filesystem"
        );
        let doc = vault2.get_document("note.md").await.unwrap();
        assert!(
            doc.to_markdown().contains("Brand new"),
            "vault2 must have the re-created content"
        );
    }

    /// A tombstone for "dir/note.md" must not block a DocumentUpdate for "note.md"
    /// (a different path at root level), and vice versa. Full-path comparison is required.
    #[tokio::test]
    async fn test_tombstone_check_uses_full_path_not_name() {
        let fs1 = InMemoryFs::new();
        let fs2 = InMemoryFs::new();

        // Vault 1 has both root-level note.md and nested/note.md; sync to vault 2.
        fs1.write("note.md", b"# Root note").await.unwrap();
        fs1.write("nested/note.md", b"# Nested note").await.unwrap();
        let vault1 = Vault::init(fs1, test_author()).await.unwrap();
        vault1.on_file_changed("note.md").await.unwrap();
        vault1.on_file_changed("nested/note.md").await.unwrap();
        let vault2 = Vault::init(fs2, test_author_2()).await.unwrap();

        full_sync(&vault2, &vault1).await;
        assert!(vault2.fs.exists("note.md").await.unwrap());
        assert!(vault2.fs.exists("nested/note.md").await.unwrap());

        // Vault 2 deletes only the root-level note.md (user removes the file, then the
        // daemon calls delete_file to tombstone it in the registry tree).
        vault2.fs.delete("note.md").await.unwrap();
        vault2.delete_file("note.md").await.unwrap();
        assert!(vault2.is_path_deleted_in_registry("note.md"), "root note.md should be deleted");
        assert!(!vault2.is_path_deleted_in_registry("nested/note.md"), "nested/note.md must NOT be deleted");

        // Vault 1 updates nested/note.md and broadcasts a DocumentUpdate.
        vault1.fs.write("nested/note.md", b"# Updated nested").await.unwrap();
        vault1.on_file_changed("nested/note.md").await.unwrap();
        let update = vault1
            .prepare_document_update("nested/note.md")
            .await
            .unwrap()
            .unwrap();

        // Vault 2 receives the update for nested/note.md — must NOT be blocked by the
        // root note.md tombstone (different full path).
        let (_, modified) = vault2.process_sync_message(&update).await.unwrap();
        assert!(
            modified.contains(&"nested/note.md".to_string()),
            "DocumentUpdate for nested/note.md must apply even though root note.md is tombstoned"
        );

        // Root note.md must still be absent (tombstone holds).
        assert!(
            !vault2.fs.exists("note.md").await.unwrap(),
            "tombstone on root note.md must not be cleared by unrelated update"
        );
    }

    /// After a daemon restart, a registry tombstone must still block resurrection.
    ///
    /// This is the production "charon recreated 12 deleted root notes" bug: the old guard
    /// lived in an in-memory session set that was empty after restart, so a stale peer's
    /// DocumentUpdate for a long-deleted path created the file as a disk orphan. The fix
    /// derives the guard from the persisted registry, so it survives the reload.
    #[tokio::test]
    async fn test_cold_restart_tombstone_blocks_resurrection() {
        use std::sync::Arc;

        let fs1 = Arc::new(InMemoryFs::new());
        let fs2 = Arc::new(InMemoryFs::new());

        // Vault 1 creates note.md and syncs it to vault 2.
        fs1.write("note.md", b"# Hello").await.unwrap();
        let vault1 = Vault::init(Arc::clone(&fs1), test_author()).await.unwrap();
        vault1.on_file_changed("note.md").await.unwrap();
        let vault2 = Vault::init(Arc::clone(&fs2), test_author_2())
            .await
            .unwrap();

        full_sync(&vault2, &vault1).await;
        assert!(fs2.exists("note.md").await.unwrap(), "setup: vault2 should have note.md");

        // Vault 2 deletes the file locally (disk delete, then registry tombstone).
        // delete_file persists the registry, so the tombstone is on fs2's disk.
        fs2.delete("note.md").await.unwrap();
        vault2.delete_file("note.md").await.unwrap();

        // Simulate a daemon restart: reload vault2 from its persisted storage. The reload
        // gets a fresh SyncState with an empty session set; the guard must instead come
        // from the registry re-imported off disk.
        drop(vault2);
        let vault2 = Vault::load(Arc::clone(&fs2), test_author_2())
            .await
            .unwrap();
        assert!(
            vault2.is_path_deleted_in_registry("note.md"),
            "deleted path must be recovered from the persisted registry after restart"
        );

        // A stale vault1 still has the file and broadcasts a DocumentUpdate.
        let update = vault1
            .prepare_document_update("note.md")
            .await
            .unwrap()
            .unwrap();

        // The reloaded vault2 must NOT resurrect the file.
        let (_, modified) = vault2.process_sync_message(&update).await.unwrap();
        assert!(
            modified.is_empty(),
            "post-restart DocumentUpdate for a deleted path must be skipped (got modified={:?})",
            modified
        );
        assert!(
            !fs2.exists("note.md").await.unwrap(),
            "deleted file must not reappear on disk after restart + inbound DocumentUpdate"
        );
    }

    /// After a restart, a peer's legitimate re-create at a previously-deleted path must
    /// still be accepted — "alive wins" survives the reload. Without it, the restored
    /// tombstone would block the new file forever.
    #[tokio::test]
    async fn test_cold_restart_alive_node_allows_create() {
        use std::sync::Arc;

        let fs1 = Arc::new(InMemoryFs::new());
        let fs2 = Arc::new(InMemoryFs::new());

        // Vault 1 creates note.md and syncs it to vault 2.
        fs1.write("note.md", b"# Hello").await.unwrap();
        let vault1 = Vault::init(Arc::clone(&fs1), test_author()).await.unwrap();
        vault1.on_file_changed("note.md").await.unwrap();
        let vault2 = Vault::init(Arc::clone(&fs2), test_author_2())
            .await
            .unwrap();

        full_sync(&vault2, &vault1).await;
        assert!(fs2.exists("note.md").await.unwrap(), "setup: vault2 should have note.md");

        // Both vaults delete note.md independently — both persist a tombstone.
        fs1.delete("note.md").await.unwrap();
        vault1.delete_file("note.md").await.unwrap();
        fs2.delete("note.md").await.unwrap();
        vault2.delete_file("note.md").await.unwrap();

        // Restart vault2 — the persisted tombstone is restored into deleted_paths.
        drop(vault2);
        let vault2 = Vault::load(Arc::clone(&fs2), test_author_2())
            .await
            .unwrap();
        assert!(
            vault2.is_path_deleted_in_registry("note.md"),
            "tombstone should be restored after restart before the re-create syncs"
        );

        // Vault 1 re-creates note.md as a brand-new alive registry node.
        fs1.write("note.md", b"# Brand new").await.unwrap();
        vault1.on_file_changed("note.md").await.unwrap();

        // Vault 1 syncs the new alive node + document to the reloaded vault2. The registry
        // import → rebuild_path_cache sees note.md alive and drops it from deleted_paths
        // (alive wins), so the document create is allowed.
        let request2 = vault2.prepare_sync_request().await.unwrap();
        let (exchange2, _) = vault1.process_sync_message(&request2).await.unwrap();
        let (_, modified) = vault2
            .process_sync_message(&exchange2.unwrap())
            .await
            .unwrap();

        assert!(
            modified.contains(&"note.md".to_string()),
            "re-create must be allowed after restart (got modified={:?})",
            modified
        );
        assert!(
            fs2.exists("note.md").await.unwrap(),
            "re-created file must appear on the reloaded vault2's filesystem"
        );
        let doc = vault2.get_document("note.md").await.unwrap();
        assert!(
            doc.to_markdown().contains("Brand new"),
            "reloaded vault2 must have the re-created content"
        );
    }

    #[tokio::test]
    async fn test_tombstone_of_cached_loser_twin_keeps_alive_path() {
        // Data-loss guard: when two ALIVE registry nodes occupy the same path
        // (a duplicate-node pair — real production debris for Log.md / Working
        // Memory.md, same FNV-1a doc_id from independent parallel indexing), a
        // tombstone arriving for the node that happens to be in the path cache
        // must NOT physically delete the .md file. The WINNER twin is still alive
        // at that path, so the file must survive.
        //
        // apply_registry_updates collects deleted_paths from the pre-import cache,
        // then rebuild_path_cache re-derives the alive-wins set. Without filtering
        // the captured vec against the rebuilt cache, the path stays in the local
        // vec (the cache held the tombstoned twin) and the file is deleted even
        // though an alive twin still occupies the path.
        use std::sync::Arc;

        let recv_fs = Arc::new(InMemoryFs::new());
        let donor_fs = Arc::new(InMemoryFs::new());

        // Receiver registers note.md → twin R.
        recv_fs.write("note.md", b"# Note").await.unwrap();
        let recv = Vault::init(Arc::clone(&recv_fs), test_author())
            .await
            .unwrap();

        // A separate vault registers the SAME path independently → twin D (a
        // different TreeID, same path/doc_id). Importing donor's registry into the
        // receiver leaves BOTH twins alive at note.md — the duplicate-node pair.
        donor_fs.write("note.md", b"# Note").await.unwrap();
        let donor = Vault::init(Arc::clone(&donor_fs), test_author_2())
            .await
            .unwrap();
        let donor_registry = donor.registry().export(loro::ExportMode::Snapshot).unwrap();
        recv.apply_registry_updates(&donor_registry).await.unwrap();

        // Precondition: two alive file nodes, both resolving to note.md.
        let alive_at_path: Vec<TreeID> = {
            let tree = recv.file_tree();
            tree.nodes()
                .into_iter()
                .filter(|id| !tree.is_node_deleted(id).unwrap_or(true))
                .filter(|id| recv.get_node_path(id).as_deref() == Some("note.md"))
                .collect()
        };
        assert_eq!(
            alive_at_path.len(),
            2,
            "setup: note.md must have two alive twins (got {:?})",
            alive_at_path
        );

        // Tombstone the twin that is CURRENTLY CACHED. The cache slot is won by
        // whichever twin iterated last in rebuild_path_cache; the FxHashMap order
        // is deterministic-per-run, so an uncontrolled fixture would pass by luck.
        // Reading the cache and tombstoning that exact node deterministically drives
        // the bug regardless of which twin won the slot.
        let cached_id = *recv.path_to_node().get("note.md").unwrap();
        let survivor_id = *alive_at_path
            .iter()
            .find(|id| **id != cached_id)
            .expect("the non-cached twin survives the tombstone");

        // Build the tombstone the production way: a peer forked from the receiver's
        // current registry deletes the cached twin, then exports. Importing that op
        // back merges a tombstone for exactly that node while the survivor stays alive.
        let tombstone_bytes = {
            let peer = loro::LoroDoc::new();
            peer.import(&recv.registry().export(loro::ExportMode::Snapshot).unwrap())
                .unwrap();
            let peer_tree = peer.get_tree(crate::vault::REGISTRY_TREE);
            peer_tree.delete(cached_id).unwrap();
            peer.export(loro::ExportMode::Snapshot).unwrap()
        };

        recv.apply_registry_updates(&tombstone_bytes).await.unwrap();

        // The survivor twin must still be alive, and the path must still resolve.
        assert!(
            !recv.file_tree().is_node_deleted(&survivor_id).unwrap_or(true),
            "the non-cached twin must remain alive after its sibling is tombstoned"
        );
        assert!(
            !recv.is_file_deleted("note.md"),
            "note.md must still resolve to an alive node (alive wins)"
        );

        // The data-loss guard: the .md file must survive because an alive twin still
        // occupies the path. Without the filter, the captured deleted_paths vec —
        // built from the cache that held the tombstoned twin — physically deletes it.
        assert!(
            recv_fs.exists("note.md").await.unwrap(),
            "note.md must NOT be physically deleted when an alive twin still occupies the path"
        );
    }
}
