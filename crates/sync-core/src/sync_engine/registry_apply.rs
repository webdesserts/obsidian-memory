use crate::fs::FileSystem;
use crate::vault::Vault;

use loro::TreeID;
use std::collections::HashMap;
use tracing::{debug, warn};

use super::{Result, SyncEngineError};

impl<F: FileSystem> Vault<F> {
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
    pub(super) async fn apply_registry_updates(&self, data: &[u8]) -> Result<Vec<String>> {
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
                crate::vault::VaultError::RegistryExport(format!("Registry export failed: {}", e))
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
}
