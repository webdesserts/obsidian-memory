//! Registry-tree operations: the LoroTree mutations that register, delete,
//! rename, and look up file nodes, plus the path-cache rebuild and sync-path
//! validation that back them.
//!
//! Extracted from `vault/mod.rs` as a sibling `impl<F: FileSystem> Vault<F>`
//! block — pure code-motion, no behavior change. Reaches the parent module's
//! `pub(crate)` items (the `TREE_META_*` / `REGISTRY_TREE` consts, `simple_hash`)
//! and the `pub(crate) sync_state` field via `super::`. The dedupe surface
//! (`find_registry_debris` / `apply_dedupe`) deliberately stays in `mod.rs`; it
//! reaches `tree_meta_string` here, which is `pub(crate)` for that reason.

use super::{
    REGISTRY_TREE, TREE_META_DOC_ID, TREE_META_NAME, TREE_META_PATH, TREE_META_TYPE, simple_hash,
};
use crate::fs::FileSystem;
use crate::vault::{Result, Vault, VaultError};
use loro::{LoroTree, TreeID, TreeParentId};
use std::collections::HashSet;

impl<F: FileSystem> Vault<F> {
    // ========== File Tree Operations (LoroTree) ==========

    /// Get the file tree from the registry
    pub(crate) fn file_tree(&self) -> LoroTree {
        self.registry().get_tree(REGISTRY_TREE)
    }

    /// Rebuild the path cache from the current tree state.
    /// Call this after applying sync updates.
    pub(crate) fn rebuild_path_cache(&self) {
        self.path_to_node_mut().clear();
        let tree = self.file_tree();

        // Deleted file paths recovered from node meta. A deleted node's real path is not
        // walkable (Loro reports its parent as `Deleted`), so we read the `path` meta
        // written at registration.
        //
        // Residual gap: the `path` meta is only written by builds that include this change,
        // so any node deleted before this build first ran on a given device carries no
        // `path` meta and is skipped here. In practice that means pre-existing production
        // tombstones are NOT guarded — only deletions made post-upgrade are recoverable by
        // path and thus protected against restart-time resurrection. This is bounded: those
        // historic tombstones have already CRDT-synced to peers, and every delete from here
        // forward is fully durable. Closing it would require a one-time backfill (out of scope).
        let mut deleted_paths: HashSet<String> = HashSet::new();

        for node_id in tree.nodes() {
            let is_deleted = tree.is_node_deleted(&node_id).unwrap_or(true);

            let Ok(meta) = tree.get_meta(node_id) else {
                continue;
            };

            let node_type = meta.get(TREE_META_TYPE).and_then(|v| {
                if let loro::ValueOrContainer::Value(val) = v {
                    val.as_string().map(|s| s.to_string())
                } else {
                    None
                }
            });

            // Only file nodes participate in the path cache and the deleted-paths guard.
            if node_type.as_deref() != Some("file") {
                continue;
            }

            if is_deleted {
                if let Some(path) = Self::tree_meta_string(&meta, TREE_META_PATH) {
                    deleted_paths.insert(path);
                }
            } else if let Some(path) = self.get_node_path(&node_id) {
                self.path_to_node_mut().insert(path, node_id);
            }
        }

        // Alive wins: a path occupied by any alive node is not deleted, regardless of any
        // duplicate-node debris or a re-create-after-delete that left an old deleted node
        // carrying the same `path` meta.
        for alive_path in self.path_to_node().keys() {
            deleted_paths.remove(alive_path);
        }

        self.sync_state.replace_deleted_paths(deleted_paths);

        tracing::debug!(
            "Rebuilt path cache with {} entries",
            self.path_to_node().len()
        );
    }

    /// Read a string-valued tree node meta field, or None if absent / not a string.
    ///
    /// `pub(crate)` (not module-private) so the dedupe surface in `mod.rs`
    /// (`find_registry_debris`), a sibling module after the registry-tree split,
    /// can still read node meta through it.
    pub(crate) fn tree_meta_string(meta: &loro::LoroMap, key: &str) -> Option<String> {
        meta.get(key).and_then(|v| {
            if let loro::ValueOrContainer::Value(val) = v {
                val.as_string().map(|s| s.to_string())
            } else {
                None
            }
        })
    }

    /// Get the path for a node by walking up the tree
    pub(crate) fn get_node_path(&self, node_id: &TreeID) -> Option<String> {
        let tree = self.file_tree();
        let mut parts = vec![];
        let mut current = *node_id;

        loop {
            // Get node metadata
            let meta = tree.get_meta(current).ok()?;
            let name = meta.get(TREE_META_NAME).and_then(|v| {
                if let loro::ValueOrContainer::Value(val) = v {
                    val.as_string().map(|s| s.to_string())
                } else {
                    None
                }
            })?;
            parts.push(name);

            // Get parent
            match tree.parent(current) {
                Some(TreeParentId::Node(parent_id)) => {
                    current = parent_id;
                }
                Some(TreeParentId::Root) | None => break,
                _ => break,
            }
        }

        parts.reverse();
        Some(parts.join("/"))
    }

    /// Find a node by path using the cache
    fn find_node_by_path(&self, path: &str) -> Option<TreeID> {
        self.path_to_node().get(path).copied()
    }

    /// Validate a sync path for security
    fn validate_sync_path(path: &str) -> Result<()> {
        // Empty path
        if path.is_empty() {
            return Err(VaultError::InvalidPath("Empty path not allowed".into()));
        }
        // Path traversal
        if path.contains("..") {
            return Err(VaultError::InvalidPath("Path traversal not allowed".into()));
        }
        // Empty segments (a//b.md)
        if path.contains("//") {
            return Err(VaultError::InvalidPath(
                "Empty path segment not allowed".into(),
            ));
        }
        // Absolute paths (Unix)
        if path.starts_with('/') {
            return Err(VaultError::InvalidPath("Absolute path not allowed".into()));
        }
        // Absolute paths (Windows - drive letter)
        if path.len() >= 2 && path.chars().nth(1) == Some(':') {
            return Err(VaultError::InvalidPath(
                "Windows absolute path not allowed".into(),
            ));
        }
        // Backslash
        if path.contains('\\') {
            return Err(VaultError::InvalidPath(
                "Backslash in path not allowed".into(),
            ));
        }
        // Null bytes
        if path.contains('\0') {
            return Err(VaultError::InvalidPath(
                "Null byte in path not allowed".into(),
            ));
        }
        // Must be .md
        if !path.ends_with(".md") {
            return Err(VaultError::InvalidPath(
                "Only markdown files allowed".into(),
            ));
        }
        // Control characters
        if path.chars().any(|c| c.is_control()) {
            return Err(VaultError::InvalidPath(
                "Control character in path not allowed".into(),
            ));
        }
        // Path length limit (filesystem safety)
        if path.len() > 1024 {
            return Err(VaultError::InvalidPath("Path too long".into()));
        }
        Ok(())
    }

    /// Register a new file in the tree (creates parent folders as needed).
    /// Returns the TreeID of the created file node.
    ///
    /// # Persistence
    ///
    /// This mutates the registry CRDT tree **in memory only**. It is sync, so it
    /// cannot flush on its own — callers own persistence. Async callers must follow
    /// it with `save_registry()` once the mutation reaches a consistent state, or the
    /// registration silently dies on the next restart. (Kept `pub` because the
    /// sync-wasm shim calls it across crate boundaries.)
    pub fn register_file(&self, path: &str) -> Result<TreeID> {
        Self::validate_sync_path(path)?;

        // Check if file already registered
        if let Some(existing_id) = self.find_node_by_path(path) {
            return Ok(existing_id);
        }

        let parts: Vec<&str> = path.split('/').collect();
        let (folders, file_name) = parts.split_at(parts.len() - 1);

        // Ensure parent folders exist
        let mut parent_id = TreeParentId::Root;
        for folder_name in folders {
            parent_id = self.get_or_create_folder(parent_id, folder_name)?;
        }

        // Create file node
        let tree = self.file_tree();
        let node_id = tree
            .create(parent_id)
            .map_err(|e| VaultError::TreeOperation(format!("Failed to create file node: {}", e)))?;

        let meta = tree
            .get_meta(node_id)
            .map_err(|e| VaultError::TreeOperation(format!("Failed to get file meta: {}", e)))?;
        meta.insert(TREE_META_TYPE, "file")
            .map_err(|e| VaultError::TreeOperation(format!("Failed to set file type: {}", e)))?;
        meta.insert(TREE_META_NAME, file_name[0])
            .map_err(|e| VaultError::TreeOperation(format!("Failed to set file name: {}", e)))?;
        meta.insert(TREE_META_DOC_ID, simple_hash(path))
            .map_err(|e| VaultError::TreeOperation(format!("Failed to set doc_id: {}", e)))?;
        // Store the full path so the node's path is recoverable after deletion.
        meta.insert(TREE_META_PATH, path)
            .map_err(|e| VaultError::TreeOperation(format!("Failed to set path meta: {}", e)))?;

        // Update cache
        self.path_to_node_mut().insert(path.to_string(), node_id);

        // A local re-create at a previously-deleted path leaves the path in deleted_paths
        // until the next rebuild_path_cache. That stale entry is harmless: on_file_changed
        // has already written the .loro and cached the document, so any inbound
        // DocumentUpdate for this path takes the exists/merge branch in apply_single_update
        // and never reaches the is_path_deleted_in_registry guard. The next
        // rebuild_path_cache then drops the path ("alive wins") as eventual cleanup.

        tracing::debug!("Registered file in tree: {}", path);
        Ok(node_id)
    }

    /// Delete a file from the tree (CRDT operation - tracked, reversible).
    /// Also cleans up the .loro document file.
    ///
    /// Returns `true` if a live tree node was tombstoned, `false` if the path was
    /// already absent from the registry (idempotent no-op). Callers can gate
    /// broadcasts on this bool without consulting the sync flag.
    pub async fn delete_file(&self, path: &str) -> Result<bool> {
        Self::validate_sync_path(path)?;

        if let Some(node_id) = self.find_node_by_path(path) {
            let tree = self.file_tree();
            tree.delete(node_id).map_err(|e| {
                VaultError::TreeOperation(format!("Failed to delete file node: {}", e))
            })?;

            // Mark the path deleted synchronously so an inbound DocumentUpdate arriving
            // before the next registry sync doesn't resurrect it. rebuild_path_cache also
            // derives this from registry truth, but it isn't called from delete_file, so
            // the synchronous insert is what guards the live-session window.
            self.mark_path_deleted(path);

            // Remove from cache
            self.path_to_node_mut().remove(path);

            // Clean up .loro document
            let sync_path = self.document_sync_path(path);
            if self.fs.exists(&sync_path).await? {
                self.fs.delete(&sync_path).await?;
            }

            // Remove from documents cache
            self.documents_mut().remove(path);

            // Persist the deletion tombstone immediately so peers importing the
            // saved registry see the op even if the process restarts before the
            // next inbound sync triggers apply_registry_updates.
            self.save_registry().await?;

            tracing::info!("Deleted file from tree: {}", path);
            Ok(true)
        } else if self.is_path_deleted_in_registry(path) {
            // A registry tombstone already covers this path, so this is a redundant
            // watcher echo for a delete that already recorded its tombstone (e.g. the
            // local watcher firing after delete_file already ran). The deletion has
            // propagated; nothing is lost, so this is debug-level noise, not a warning.
            tracing::debug!(
                "delete_file: '{}' already tombstoned — redundant delete, no-op",
                path
            );
            Ok(false)
        } else {
            // Genuinely unknown path: no node at all (alive or tombstoned), so the
            // deletion records no tombstone and won't propagate to peers. The `false`
            // return carries this diagnostic for daemon callers, but the warn stays for
            // non-daemon callers that ignore the bool.
            tracing::warn!(
                "delete_file: no registry node for '{}' — no tombstone recorded",
                path
            );
            Ok(false)
        }
    }

    /// Rename/move a file in the tree (CRDT operation via tree move).
    pub async fn rename_file(&self, old_path: &str, new_path: &str) -> Result<()> {
        Self::validate_sync_path(old_path)?;
        Self::validate_sync_path(new_path)?;

        // No-op if paths are identical
        if old_path == new_path {
            return Ok(());
        }

        let Some(node_id) = self.find_node_by_path(old_path) else {
            // Source not in tree - this can happen when receiving FileRenamed before
            // the registry has synced. Handle the rename at filesystem level if possible.
            if self.fs.exists(old_path).await.unwrap_or(false) {
                // Source exists on disk but not in tree - rename on disk and register target
                tracing::debug!(
                    "rename_file: source {} not in tree but exists on disk - renaming and registering",
                    old_path
                );

                // Rename the actual file
                let content = self.fs.read(old_path).await?;
                self.fs.write(new_path, &content).await?;
                self.fs.delete(old_path).await?;

                // Move .loro file if it exists
                let old_sync = self.document_sync_path(old_path);
                let new_sync = self.document_sync_path(new_path);
                if self.fs.exists(&old_sync).await.unwrap_or(false) {
                    let sync_content = self.fs.read(&old_sync).await?;
                    self.fs.write(&new_sync, &sync_content).await?;
                    self.fs.delete(&old_sync).await?;
                }

                // Update documents cache - extract first to release mutex before re-acquiring
                let doc = self.documents_mut().remove(old_path);
                if let Some(doc) = doc {
                    self.documents_mut().insert(new_path.to_string(), doc);
                }

                // Register in tree and persist
                self.register_file(new_path)?;
                self.save_registry().await?;
                return Ok(());
            } else if self.fs.exists(new_path).await.unwrap_or(false) {
                // Target already exists (rename already happened) - just register it
                tracing::debug!(
                    "rename_file: source {} not in tree, but {} exists - registering target",
                    old_path,
                    new_path
                );
                self.register_file(new_path)?;
                self.save_registry().await?;

                // Clean up orphaned .loro at old path if it exists
                let old_sync = self.document_sync_path(old_path);
                if self.fs.exists(&old_sync).await.unwrap_or(false) {
                    let _ = self.fs.delete(&old_sync).await;
                }

                return Ok(());
            }
            return Err(VaultError::RenameSourceMissing(format!(
                "Source file not found: {}",
                old_path
            )));
        };

        // Check target doesn't exist
        if self.find_node_by_path(new_path).is_some() {
            return Err(VaultError::RenameTargetExists(format!(
                "Target already exists: {}",
                new_path
            )));
        }

        let new_parts: Vec<&str> = new_path.split('/').collect();
        let (new_folders, new_name) = new_parts.split_at(new_parts.len() - 1);

        // Ensure new parent folders exist
        let mut new_parent = TreeParentId::Root;
        for folder_name in new_folders {
            new_parent = self.get_or_create_folder(new_parent, folder_name)?;
        }

        let tree = self.file_tree();

        // Move node to new parent (Loro API is `mov`)
        tree.mov(node_id, new_parent)
            .map_err(|e| VaultError::TreeOperation(format!("Failed to move file node: {}", e)))?;

        // Update name in metadata
        let meta = tree
            .get_meta(node_id)
            .map_err(|e| VaultError::TreeOperation(format!("Failed to get file meta: {}", e)))?;
        meta.insert(TREE_META_NAME, new_name[0])
            .map_err(|e| VaultError::TreeOperation(format!("Failed to update file name: {}", e)))?;
        meta.insert(TREE_META_DOC_ID, simple_hash(new_path))
            .map_err(|e| VaultError::TreeOperation(format!("Failed to update doc_id: {}", e)))?;
        // This main path moves the node via tree.mov() rather than register_file, so the
        // path meta must be updated explicitly to keep the deleted-path guard accurate if
        // the renamed node is later deleted.
        meta.insert(TREE_META_PATH, new_path)
            .map_err(|e| VaultError::TreeOperation(format!("Failed to update path meta: {}", e)))?;

        // Update caches
        self.path_to_node_mut().remove(old_path);
        self.path_to_node_mut()
            .insert(new_path.to_string(), node_id);

        // Move .loro document file
        let old_sync_path = self.document_sync_path(old_path);
        let new_sync_path = self.document_sync_path(new_path);
        if self.fs.exists(&old_sync_path).await? {
            let bytes = self.fs.read(&old_sync_path).await?;
            self.fs.atomic_write(&new_sync_path, &bytes).await?;
            self.fs.delete(&old_sync_path).await?;
        }

        // Update documents cache - extract first to release mutex before re-acquiring
        let doc = self.documents_mut().remove(old_path);
        if let Some(doc) = doc {
            self.documents_mut().insert(new_path.to_string(), doc);
        }

        // Persist the move op so restarts see the updated registry.
        self.save_registry().await?;

        tracing::info!("Renamed file in tree: {} -> {}", old_path, new_path);
        Ok(())
    }

    /// Check if a file is deleted in the tree
    pub fn is_file_deleted(&self, path: &str) -> bool {
        match self.find_node_by_path(path) {
            Some(node_id) => {
                let tree = self.file_tree();
                tree.is_node_deleted(&node_id).unwrap_or(true)
            }
            None => true, // Not in tree = effectively deleted
        }
    }

    /// Get or create a folder node
    fn get_or_create_folder(&self, parent: TreeParentId, name: &str) -> Result<TreeParentId> {
        let tree = self.file_tree();

        // Look for existing folder with this name under parent
        let children = match &parent {
            TreeParentId::Root => tree.roots(),
            TreeParentId::Node(parent_id) => tree.children(parent_id).unwrap_or_default(),
            _ => vec![],
        };

        for child_id in children {
            if let Ok(meta) = tree.get_meta(child_id) {
                let is_folder = meta
                    .get("type")
                    .and_then(|v| {
                        if let loro::ValueOrContainer::Value(val) = v {
                            val.as_string().map(|s| s.as_ref() == "folder")
                        } else {
                            None
                        }
                    })
                    .unwrap_or(false);

                let child_name = meta.get(TREE_META_NAME).and_then(|v| {
                    if let loro::ValueOrContainer::Value(val) = v {
                        val.as_string().map(|s| s.to_string())
                    } else {
                        None
                    }
                });

                if is_folder && child_name.as_deref() == Some(name) {
                    return Ok(TreeParentId::Node(child_id));
                }
            }
        }

        // Create new folder node
        let node_id = tree.create(parent).map_err(|e| {
            VaultError::TreeOperation(format!("Failed to create folder node: {}", e))
        })?;

        let meta = tree
            .get_meta(node_id)
            .map_err(|e| VaultError::TreeOperation(format!("Failed to get folder meta: {}", e)))?;
        meta.insert(TREE_META_TYPE, "folder")
            .map_err(|e| VaultError::TreeOperation(format!("Failed to set folder type: {}", e)))?;
        meta.insert(TREE_META_NAME, name)
            .map_err(|e| VaultError::TreeOperation(format!("Failed to set folder name: {}", e)))?;

        Ok(TreeParentId::Node(node_id))
    }
}
