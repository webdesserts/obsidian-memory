//! Tree operations on the Index: the `LoroTree` mutations that register, move,
//! and delete file nodes, the lookups that back them, and the cache rebuild and
//! path validation they rely on.
//!
//! Carried from `sync-core`'s `vault/registry_tree.rs` as an `impl Index` block.
//! Every operation here is an in-memory CRDT mutation — there is no filesystem
//! coupling. Three deliberate departures from the port:
//! - **UUID identity:** a file node carries a `uuid` meta (the document's minted
//!   identity) plus a denormalized `content_version`, instead of the old
//!   path-hash `doc_id`. The UUID is never recomputed.
//! - **Two-way caches:** the rebuild fills both `path_to_node` and the inverse
//!   `uuid_to_node` in its single walk over alive file nodes.
//! - **Pure-structural move:** `move_node` does only `tree.mov` + meta updates +
//!   cache updates. The old `rename_file`'s content-`.loro` relocation dance is
//!   gone (the content file is `docs/<uuid>.loro`, path-independent), and its
//!   fs-level recovery fallback (reading/writing `.md` when the source isn't in the
//!   tree) belongs to the public handle's flows, not the catalog.

use super::{
    INDEX_TREE, Index, TREE_META_CONTENT_VERSION, TREE_META_NAME, TREE_META_PATH, TREE_META_TYPE,
    TREE_META_UUID,
};
use crate::index::state::{IndexError, Result};
use loro::{LoroTree, TreeID, TreeParentId};
use std::collections::HashSet;
use uuid::Uuid;

/// One alive node in the merged tree, paired with its display path — the raw input
/// the conflict resolver's view ([`crate::conflict::StructuralView`]) is built from.
///
/// Deliberately content-free: the resolver needs a file node's [`ContentSummary`]
/// (which requires an async content-doc load), but the Index is fs-agnostic, so the
/// scan returns only the node's identity and the Vault enriches files with their
/// summary in the apply path. A folder carries only its `TreeID` (folders have no
/// content-UUID — their survivor key is their tree-node identity).
///
/// [`ContentSummary`]: crate::hash::ContentSummary
#[derive(Debug, Clone)]
pub enum StructuralNode {
    /// An alive file node at `path`, identified by its content `uuid`.
    File { path: String, uuid: Uuid },
    /// An alive folder node at `path`, identified by its loro `tree_id`.
    Folder { path: String, tree_id: TreeID },
}

impl StructuralNode {
    /// The node's display path — for grouping nodes by path to detect collisions.
    pub fn path(&self) -> &str {
        match self {
            StructuralNode::File { path, .. } | StructuralNode::Folder { path, .. } => path,
        }
    }
}

impl Index {
    // ========== File tree operations (LoroTree) ==========

    /// Get the file tree from the index document.
    pub(crate) fn index_tree(&self) -> LoroTree {
        self.index().get_tree(INDEX_TREE)
    }

    /// Rebuild both lookup caches from the current tree state.
    ///
    /// Call this after applying sync updates (or on load). It walks every alive
    /// file node once, filling `path_to_node` and the inverse `uuid_to_node`, and
    /// rederives the deleted-paths guard from the tombstoned nodes' `path` meta.
    pub fn rebuild_caches(&self) {
        self.path_to_node_mut().clear();
        self.uuid_to_node_mut().clear();
        let tree = self.index_tree();

        // Deleted file paths recovered from node meta. A deleted node's real path is
        // not walkable (Loro reports its parent as `Deleted`), so we read the `path`
        // meta written at registration.
        let mut deleted_paths: HashSet<String> = HashSet::new();

        for node_id in tree.nodes() {
            let is_deleted = tree.is_node_deleted(&node_id).unwrap_or(true);

            let Ok(meta) = tree.get_meta(node_id) else {
                continue;
            };

            let node_type = Self::tree_meta_string(&meta, TREE_META_TYPE);

            // Only file nodes participate in the caches and the deleted-paths guard.
            if node_type.as_deref() != Some("file") {
                continue;
            }

            if is_deleted {
                if let Some(path) = Self::tree_meta_string(&meta, TREE_META_PATH) {
                    deleted_paths.insert(path);
                }
            } else if let Some(path) = self.get_node_path(&node_id) {
                // Fill the path cache. Also fill the inverse uuid cache when the
                // node carries a parseable UUID (every node registered through
                // `register_document` does).
                if let Some(uuid) = Self::tree_meta_string(&meta, TREE_META_UUID)
                    .and_then(|s| Uuid::parse_str(&s).ok())
                {
                    self.uuid_to_node_mut().insert(uuid, node_id);
                }
                self.path_to_node_mut().insert(path, node_id);
            }
        }

        // Alive wins: a path occupied by any alive node is not deleted, regardless
        // of a re-create-after-delete that left an old deleted node carrying the
        // same `path` meta.
        for alive_path in self.path_to_node().keys() {
            deleted_paths.remove(alive_path);
        }

        self.sync_state.replace_deleted_paths(deleted_paths);

        tracing::debug!(
            "Rebuilt index caches: {} path entries, {} uuid entries",
            self.path_to_node().len(),
            self.uuid_to_node().len()
        );
    }

    /// Scan every **alive** node in the merged tree into a flat list of
    /// `(path, identity)` records — the raw input for the conflict resolver's view.
    ///
    /// Unlike the caches (which key uniquely per path, so a collision's *loser* has
    /// no slot, and which omit folders entirely), this walks `tree.nodes()` directly,
    /// so it surfaces EVERY alive node at a contested path — both files at a file
    /// collision, both folders at a folder collision, and a file-and-folder pair at a
    /// file-vs-folder collision. That completeness is exactly what the cascade needs:
    /// the rebuilt caches can't see the nodes it must resolve.
    ///
    /// A file node yields its `uuid`; a folder yields its `TreeID`. The Vault enriches
    /// each file with its [`ContentSummary`] (an async content-doc load it owns) to
    /// build the full [`StructuralView`]. Nodes whose path or identity can't be read
    /// are skipped (defensive — a malformed node can't participate in resolution).
    ///
    /// [`ContentSummary`]: crate::hash::ContentSummary
    /// [`StructuralView`]: crate::conflict::StructuralView
    pub fn scan_structural_nodes(&self) -> Vec<StructuralNode> {
        let tree = self.index_tree();
        let mut nodes = Vec::new();

        for node_id in tree.nodes() {
            if tree.is_node_deleted(&node_id).unwrap_or(true) {
                continue;
            }
            let Ok(meta) = tree.get_meta(node_id) else {
                continue;
            };
            let Some(path) = self.get_node_path(&node_id) else {
                continue;
            };
            match Self::tree_meta_string(&meta, TREE_META_TYPE).as_deref() {
                Some("file") => {
                    if let Some(uuid) = Self::tree_meta_string(&meta, TREE_META_UUID)
                        .and_then(|s| Uuid::parse_str(&s).ok())
                    {
                        nodes.push(StructuralNode::File { path, uuid });
                    }
                }
                Some("folder") => {
                    nodes.push(StructuralNode::Folder {
                        path,
                        tree_id: node_id,
                    });
                }
                _ => {}
            }
        }

        nodes
    }

    /// Read a string-valued tree node meta field, or `None` if absent / not a string.
    pub(crate) fn tree_meta_string(meta: &loro::LoroMap, key: &str) -> Option<String> {
        meta.get(key).and_then(|v| {
            if let loro::ValueOrContainer::Value(val) = v {
                val.as_string().map(|s| s.to_string())
            } else {
                None
            }
        })
    }

    /// Get the full path for a node by walking up the tree.
    pub(crate) fn get_node_path(&self, node_id: &TreeID) -> Option<String> {
        let tree = self.index_tree();
        let mut parts = vec![];
        let mut current = *node_id;

        loop {
            let meta = tree.get_meta(current).ok()?;
            let name = Self::tree_meta_string(&meta, TREE_META_NAME)?;
            parts.push(name);

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

    /// Find a node by path using the cache.
    fn find_node_by_path(&self, path: &str) -> Option<TreeID> {
        self.path_to_node().get(path).copied()
    }

    /// Find the node currently at `path`, if any (a forward-cache lookup).
    ///
    /// The public read for "does the index have a live document at this path?" — the
    /// resolution a filesystem watcher event needs.
    pub fn node_for_path(&self, path: &str) -> Option<TreeID> {
        self.find_node_by_path(path)
    }

    /// Find a node by document UUID using the inverse cache.
    ///
    /// The resolution an inbound wire `DocUpdate{uuid}` needs: which node (and
    /// thence which current path) does this document live at?
    pub fn find_node_by_uuid(&self, uuid: &Uuid) -> Option<TreeID> {
        self.uuid_to_node().get(uuid).copied()
    }

    /// The current vault-relative path of a node (walks the tree up to the root).
    ///
    /// Pairs with [`Self::find_node_by_uuid`] to complete the inbound-wire
    /// resolution `uuid → node → path`.
    pub fn path_for_node(&self, node_id: &TreeID) -> Option<String> {
        self.get_node_path(node_id)
    }

    /// The document UUID stored on a node, if it is a file node with a parseable
    /// `uuid` meta.
    pub fn node_uuid(&self, node_id: &TreeID) -> Option<Uuid> {
        let meta = self.index_tree().get_meta(*node_id).ok()?;
        Self::tree_meta_string(&meta, TREE_META_UUID).and_then(|s| Uuid::parse_str(&s).ok())
    }

    /// The path a *tombstoned* file node carrying `uuid` was deleted at — the
    /// orphan's last-known location, recovered for the native-move adopt (OQ-3).
    ///
    /// A move arrives at boot as a `delete(old)` + `create(new)`: the old node is
    /// tombstoned but its `path` meta still reads `old` and its `uuid` meta still
    /// reads the moved document's UUID. The live caches deliberately exclude
    /// tombstoned nodes, so this walks the tree to find the deleted node. It is the
    /// replacement for the deleted `_meta.path` in the content doc — the content doc
    /// is now location-agnostic, so the move's old path is recovered HERE (from the
    /// Index), not from the content doc.
    ///
    /// Returns `None` when no tombstoned file node carries that UUID (e.g. an
    /// orphaned content doc whose node never existed — the fs↔loro divergence case,
    /// which is healed by the divergence-adopt arm rather than the move-adopt arm).
    pub fn deleted_node_path_for_uuid(&self, uuid: &Uuid) -> Option<String> {
        let tree = self.index_tree();
        for node_id in tree.nodes() {
            // Only a tombstoned node is a candidate — a live node carrying this UUID
            // is resolvable through the caches and is not an orphan.
            if !tree.is_node_deleted(&node_id).unwrap_or(true) {
                continue;
            }
            let Ok(meta) = tree.get_meta(node_id) else {
                continue;
            };
            if Self::tree_meta_string(&meta, TREE_META_TYPE).as_deref() != Some("file") {
                continue;
            }
            let matches_uuid = Self::tree_meta_string(&meta, TREE_META_UUID)
                .and_then(|s| Uuid::parse_str(&s).ok())
                .as_ref()
                == Some(uuid);
            if matches_uuid {
                return Self::tree_meta_string(&meta, TREE_META_PATH);
            }
        }
        None
    }

    /// The denormalized `content_version` fingerprint stored on a node, if present.
    ///
    /// A derived cache (the content doc's `state_vv()` is authoritative); the
    /// compare protocol reads this to digest the catalog without opening content
    /// docs.
    pub fn node_content_version(&self, node_id: &TreeID) -> Option<[u8; 32]> {
        let meta = self.index_tree().get_meta(*node_id).ok()?;
        let value = meta.get(TREE_META_CONTENT_VERSION)?;
        let loro::ValueOrContainer::Value(loro::LoroValue::Binary(bytes)) = value else {
            return None;
        };
        bytes.as_slice().try_into().ok()
    }

    /// Refresh a node's denormalized `content_version` fingerprint after a local edit.
    ///
    /// `register_document` sets the initial value; the public handle's local-write
    /// flow calls this on each real content change so the derived cache stays in step
    /// with the content doc's `state_vv()` (the authoritative source). In-memory only
    /// — the caller persists via `save_index`.
    pub fn set_content_version(&self, node_id: &TreeID, content_version: &[u8; 32]) -> Result<()> {
        let meta = self.index_tree().get_meta(*node_id).map_err(|e| {
            IndexError::TreeOperation(format!(
                "Failed to get file meta for content_version: {}",
                e
            ))
        })?;
        meta.insert(TREE_META_CONTENT_VERSION, content_version.as_slice())
            .map_err(|e| {
                IndexError::TreeOperation(format!("Failed to update content_version: {}", e))
            })?;
        Ok(())
    }

    /// Validate a sync path for security.
    fn validate_sync_path(path: &str) -> Result<()> {
        // Empty path
        if path.is_empty() {
            return Err(IndexError::InvalidPath("Empty path not allowed".into()));
        }
        // Path traversal
        if path.contains("..") {
            return Err(IndexError::InvalidPath("Path traversal not allowed".into()));
        }
        // Empty segments (a//b.md)
        if path.contains("//") {
            return Err(IndexError::InvalidPath(
                "Empty path segment not allowed".into(),
            ));
        }
        // Absolute paths (Unix)
        if path.starts_with('/') {
            return Err(IndexError::InvalidPath("Absolute path not allowed".into()));
        }
        // Absolute paths (Windows - drive letter)
        if path.len() >= 2 && path.chars().nth(1) == Some(':') {
            return Err(IndexError::InvalidPath(
                "Windows absolute path not allowed".into(),
            ));
        }
        // Backslash
        if path.contains('\\') {
            return Err(IndexError::InvalidPath(
                "Backslash in path not allowed".into(),
            ));
        }
        // Null bytes
        if path.contains('\0') {
            return Err(IndexError::InvalidPath(
                "Null byte in path not allowed".into(),
            ));
        }
        // Must be .md
        if !path.ends_with(".md") {
            return Err(IndexError::InvalidPath(
                "Only markdown files allowed".into(),
            ));
        }
        // Control characters
        if path.chars().any(|c| c.is_control()) {
            return Err(IndexError::InvalidPath(
                "Control character in path not allowed".into(),
            ));
        }
        // Path length limit (filesystem safety)
        if path.len() > 1024 {
            return Err(IndexError::InvalidPath("Path too long".into()));
        }
        Ok(())
    }

    /// Register a document in the index (creating parent folders as needed).
    /// Returns the `TreeID` of the file node.
    ///
    /// The caller supplies the document's `uuid` (its minted identity, read from
    /// the content doc's `_meta.doc_id`) and a `content_version` fingerprint of the
    /// content doc's current version vector. The Index stores both verbatim — it
    /// does not open content docs. The UUID becomes the node's permanent identity.
    ///
    /// In-memory only: the caller is responsible for persisting via `save_index`
    /// once the mutation reaches a consistent state.
    pub fn register_document(
        &self,
        path: &str,
        uuid: &Uuid,
        content_version: &[u8; 32],
    ) -> Result<TreeID> {
        Self::validate_sync_path(path)?;

        // Already registered at this path → idempotent return of the existing node.
        if let Some(existing_id) = self.find_node_by_path(path) {
            return Ok(existing_id);
        }

        let parts: Vec<&str> = path.split('/').collect();
        let (folders, file_name) = parts.split_at(parts.len() - 1);

        // Ensure parent folders exist.
        let mut parent_id = TreeParentId::Root;
        for folder_name in folders {
            parent_id = self.get_or_create_folder(parent_id, folder_name)?;
        }

        // Create the file node.
        let tree = self.index_tree();
        let node_id = tree
            .create(parent_id)
            .map_err(|e| IndexError::TreeOperation(format!("Failed to create file node: {}", e)))?;

        let meta = tree
            .get_meta(node_id)
            .map_err(|e| IndexError::TreeOperation(format!("Failed to get file meta: {}", e)))?;
        meta.insert(TREE_META_TYPE, "file")
            .map_err(|e| IndexError::TreeOperation(format!("Failed to set file type: {}", e)))?;
        meta.insert(TREE_META_NAME, file_name[0])
            .map_err(|e| IndexError::TreeOperation(format!("Failed to set file name: {}", e)))?;
        // The document's UUID identity — written once, never recomputed (a move
        // does not touch it). This is what keeps `docs/<uuid>.loro` stable across
        // renames.
        meta.insert(TREE_META_UUID, uuid.to_string())
            .map_err(|e| IndexError::TreeOperation(format!("Failed to set uuid: {}", e)))?;
        // The denormalized version fingerprint (a derived cache; the content doc's
        // `state_vv()` is authoritative). Stored as the raw 32 bytes.
        meta.insert(TREE_META_CONTENT_VERSION, content_version.as_slice())
            .map_err(|e| {
                IndexError::TreeOperation(format!("Failed to set content_version: {}", e))
            })?;
        // Store the full path so the node's path is recoverable after deletion.
        meta.insert(TREE_META_PATH, path)
            .map_err(|e| IndexError::TreeOperation(format!("Failed to set path meta: {}", e)))?;

        // Update both caches.
        self.path_to_node_mut().insert(path.to_string(), node_id);
        self.uuid_to_node_mut().insert(*uuid, node_id);

        tracing::debug!("Registered document in index: {} ({})", path, uuid);
        Ok(node_id)
    }

    /// Delete a node from the index (a CRDT tombstone — tracked, reversible).
    ///
    /// Returns `true` if a live tree node was tombstoned, `false` if the path was
    /// already absent (an idempotent no-op). Callers can gate broadcasts on this
    /// bool. In-memory only — the caller persists via `save_index`; the content
    /// `.loro` cleanup (if any) is a flow concern, not the catalog's.
    pub fn delete_node(&self, path: &str) -> Result<bool> {
        Self::validate_sync_path(path)?;

        if let Some(node_id) = self.find_node_by_path(path) {
            let tree = self.index_tree();
            tree.delete(node_id).map_err(|e| {
                IndexError::TreeOperation(format!("Failed to delete file node: {}", e))
            })?;

            // Mark the path deleted synchronously so an inbound document update
            // arriving before the next index sync doesn't resurrect it.
            self.sync_state.mark_path_deleted(path);

            // Remove from both caches. The uuid entry is removed by node id since we
            // resolved the node from the path.
            self.path_to_node_mut().remove(path);
            self.uuid_to_node_mut().retain(|_, id| *id != node_id);

            tracing::info!("Deleted node from index: {}", path);
            Ok(true)
        } else if self.sync_state.is_path_deleted_in_index(path) {
            // A tombstone already covers this path — a redundant echo for a delete
            // that already recorded its tombstone. Nothing is lost.
            tracing::debug!(
                "delete_node: '{}' already tombstoned — redundant delete, no-op",
                path
            );
            Ok(false)
        } else {
            // Genuinely unknown path: no node at all, so no tombstone is recorded
            // and nothing propagates to peers.
            tracing::warn!(
                "delete_node: no index node for '{}' — no tombstone recorded",
                path
            );
            Ok(false)
        }
    }

    /// Tombstone a SPECIFIC node by its `TreeID` — the conflict cascade's collapse
    /// primitive.
    ///
    /// Unlike [`Self::delete_node`] (which resolves the node from `path_to_node`), this
    /// targets an exact node id, because the cascade operates precisely when TWO file
    /// nodes share one display path: there the path cache holds only ONE of them, so a
    /// path-keyed delete could tombstone the wrong one (the survivor instead of the
    /// loser). The caller resolves `uuid → TreeID` via [`Self::find_node_by_uuid`]
    /// (which keys correctly per UUID) and passes that id here.
    ///
    /// `path` is the node's current display path (the caller already resolved it),
    /// used to arm the deleted-paths guard and to patch the caches. In-memory only —
    /// the caller persists via `save_index` and typically rebuilds caches afterward.
    pub fn delete_node_by_id(&self, node_id: TreeID, path: &str) -> Result<()> {
        let tree = self.index_tree();
        tree.delete(node_id).map_err(|e| {
            IndexError::TreeOperation(format!("Failed to delete file node by id: {}", e))
        })?;

        // Arm the deleted-paths guard for this path so an in-flight document update
        // doesn't resurrect it. Note: a `rebuild_caches` with alive-wins will lift this
        // again if the SURVIVOR still occupies the same path — which is exactly right
        // (the path is not deleted, just the losing node).
        self.sync_state.mark_path_deleted(path);

        // Drop this node from both caches by id (the path cache may currently point at
        // the survivor's node, so remove by VALUE, not by the path key).
        self.path_to_node_mut().retain(|_, id| *id != node_id);
        self.uuid_to_node_mut().retain(|_, id| *id != node_id);

        tracing::info!("Deleted node from index by id: {} ({:?})", path, node_id);
        Ok(())
    }

    /// Move a SPECIFIC node by its `TreeID` to `new_path` — the conflict cascade's
    /// rename primitive.
    ///
    /// Unlike [`Self::move_node`] (which resolves the source from `path_to_node`), this
    /// targets an exact node id, because the cascade renames a loser whose OLD path is
    /// shared with the survivor — a path-keyed move could relocate the survivor
    /// instead. The caller resolves `uuid → TreeID` (the correct per-UUID lookup) and
    /// passes the id plus the node's current `old_path`.
    ///
    /// Errors with `MoveTargetExists` if `new_path` already has a node — the caller
    /// (the cascade) treats that as a resolver bug and fails loudly (S1). The node's
    /// `uuid` is untouched (identity is stable across the rename). In-memory only.
    pub fn move_node_by_id(&self, node_id: TreeID, old_path: &str, new_path: &str) -> Result<()> {
        Self::validate_sync_path(new_path)?;
        if old_path == new_path {
            return Ok(());
        }
        if self.find_node_by_path(new_path).is_some() {
            return Err(IndexError::MoveTargetExists(format!(
                "Target already exists: {}",
                new_path
            )));
        }

        let new_parts: Vec<&str> = new_path.split('/').collect();
        let (new_folders, new_name) = new_parts.split_at(new_parts.len() - 1);

        let mut new_parent = TreeParentId::Root;
        for folder_name in new_folders {
            new_parent = self.get_or_create_folder(new_parent, folder_name)?;
        }

        let tree = self.index_tree();
        tree.mov(node_id, new_parent).map_err(|e| {
            IndexError::TreeOperation(format!("Failed to move file node by id: {}", e))
        })?;

        let meta = tree
            .get_meta(node_id)
            .map_err(|e| IndexError::TreeOperation(format!("Failed to get file meta: {}", e)))?;
        meta.insert(TREE_META_NAME, new_name[0])
            .map_err(|e| IndexError::TreeOperation(format!("Failed to update file name: {}", e)))?;
        meta.insert(TREE_META_PATH, new_path)
            .map_err(|e| IndexError::TreeOperation(format!("Failed to update path meta: {}", e)))?;

        // Patch the path cache: drop the old key only if it points at THIS node (the
        // survivor may own it), and point the new key at this node.
        self.path_to_node_mut().retain(|_, id| *id != node_id);
        self.path_to_node_mut()
            .insert(new_path.to_string(), node_id);

        tracing::info!(
            "Moved node in index by id: {} -> {} ({:?})",
            old_path,
            new_path,
            node_id
        );
        Ok(())
    }

    /// Move/rename a node in the index — a pure-structural CRDT tree move.
    ///
    /// Does `tree.mov` + `name`/`path` meta updates + both-cache updates, and
    /// **nothing to the content `.loro`**: the content file is `docs/<uuid>.loro`,
    /// path-independent, so a move re-transfers zero content (INV-1). The node's
    /// `uuid` is NOT touched — identity is stable across a move.
    ///
    /// In-memory only — the caller persists via `save_index`. If the source path
    /// has no node, this errors; the fs-level recovery the old `rename_file` did
    /// (renaming a `.md` that exists on disk but not in the tree) is a flow concern
    /// the public handle owns, not the catalog.
    pub fn move_node(&self, old_path: &str, new_path: &str) -> Result<()> {
        Self::validate_sync_path(old_path)?;
        Self::validate_sync_path(new_path)?;

        // No-op if paths are identical.
        if old_path == new_path {
            return Ok(());
        }

        let Some(node_id) = self.find_node_by_path(old_path) else {
            return Err(IndexError::MoveSourceMissing(format!(
                "Source node not found: {}",
                old_path
            )));
        };

        // Target must not already exist.
        if self.find_node_by_path(new_path).is_some() {
            return Err(IndexError::MoveTargetExists(format!(
                "Target already exists: {}",
                new_path
            )));
        }

        let new_parts: Vec<&str> = new_path.split('/').collect();
        let (new_folders, new_name) = new_parts.split_at(new_parts.len() - 1);

        // Ensure the new parent folders exist.
        let mut new_parent = TreeParentId::Root;
        for folder_name in new_folders {
            new_parent = self.get_or_create_folder(new_parent, folder_name)?;
        }

        let tree = self.index_tree();

        // Move the node to its new parent (Loro API is `mov`).
        tree.mov(node_id, new_parent)
            .map_err(|e| IndexError::TreeOperation(format!("Failed to move file node: {}", e)))?;

        // Update the name and path meta. The UUID and content_version are untouched
        // — a move changes location, not identity or content.
        let meta = tree
            .get_meta(node_id)
            .map_err(|e| IndexError::TreeOperation(format!("Failed to get file meta: {}", e)))?;
        meta.insert(TREE_META_NAME, new_name[0])
            .map_err(|e| IndexError::TreeOperation(format!("Failed to update file name: {}", e)))?;
        meta.insert(TREE_META_PATH, new_path)
            .map_err(|e| IndexError::TreeOperation(format!("Failed to update path meta: {}", e)))?;

        // Update the path cache (the uuid cache's node id is unchanged by a move).
        self.path_to_node_mut().remove(old_path);
        self.path_to_node_mut()
            .insert(new_path.to_string(), node_id);

        tracing::info!("Moved node in index: {} -> {}", old_path, new_path);
        Ok(())
    }

    /// Move/rename a whole FOLDER and everything under it — the folder-safe move
    /// primitive (INV-1.5b) that the per-file [`Self::move_node`] is not.
    ///
    /// A folder move is ONE `tree.mov` on the folder node: loro's movable tree
    /// carries every descendant structurally for free, so no content `.loro` is
    /// touched and every descendant keeps its UUID — that is what makes "a folder
    /// move re-transfers zero content" (INV-1) structurally true.
    ///
    /// The reason this can't just be `move_node` on the folder: `move_node` rewrites
    /// only the single moved node's denormalized `path`/`name` meta and patches one
    /// cache entry. After re-parenting a folder, every DESCENDANT file node's `path`
    /// meta and `path_to_node` entry is stale — its tree position moved but its
    /// stored path string didn't. That denormalized `path` meta is load-bearing: it's
    /// the only way a tombstoned node's location is recovered
    /// ([`Self::deleted_node_path_for_uuid`] + the deleted-paths re-derivation in
    /// [`Self::rebuild_caches`]). So this rewrites every descendant file node's
    /// `path` meta to its new location, then rebuilds the caches from index truth.
    ///
    /// Correctness-first: it does the single `tree.mov`, rewrites descendant `path`
    /// meta, then a full [`Self::rebuild_caches`] (already O(nodes) on every load), in
    /// that order. Folder descendants carry only `type`+`name` meta (no `path`), so
    /// they need no rewrite — the rebuild re-derives their paths from the tree.
    ///
    /// This is a clean STRUCTURAL mover: it errors with [`IndexError::MoveTargetExists`]
    /// if `new_prefix` is already occupied by a folder or a file. Collision POLICY
    /// (folder-merge, file-vs-folder) lives in the conflict resolver, not here. In-memory
    /// only — the caller persists via [`Self::save_index`].
    pub fn move_subtree(&self, old_prefix: &str, new_prefix: &str) -> Result<()> {
        // No-op if the prefixes are identical (mirrors `move_node`).
        if old_prefix == new_prefix {
            return Ok(());
        }

        // Resolve the folder node at `old_prefix`. Folders are not in `path_to_node`,
        // so descend the prefix's segments from the root, requiring each to be an
        // existing folder. A missing folder node means there is nothing to move.
        let Some(folder_node) = self.find_folder_node(old_prefix) else {
            return Err(IndexError::MoveSourceMissing(format!(
                "Source folder not found: {}",
                old_prefix
            )));
        };

        // The raw primitive refuses an occupied target — `new_prefix` already taken by
        // a folder (folder-MERGE territory) or a file (file-vs-folder). Collision
        // policy is the resolver's job (P2d); here it's a hard error. Checked BEFORE
        // creating the destination parent chain so a refused move leaves no spurious
        // folders behind.
        if self.find_folder_node(new_prefix).is_some()
            || self.find_node_by_path(new_prefix).is_some()
        {
            return Err(IndexError::MoveTargetExists(format!(
                "Target already exists: {}",
                new_prefix
            )));
        }

        // Ensure the destination PARENT chain exists (everything above the folder's
        // own new name), then re-parent the folder node there in one move.
        let new_parts: Vec<&str> = new_prefix.split('/').collect();
        let (new_parent_folders, new_name) = new_parts.split_at(new_parts.len() - 1);
        let mut new_parent = TreeParentId::Root;
        for folder_name in new_parent_folders {
            new_parent = self.get_or_create_folder(new_parent, folder_name)?;
        }

        let tree = self.index_tree();
        tree.mov(folder_node, new_parent)
            .map_err(|e| IndexError::TreeOperation(format!("Failed to move folder node: {}", e)))?;

        // Rename the folder node itself to the new last segment. Descendants follow
        // structurally, so their `name` meta is unchanged — only the moved folder's
        // own name changes.
        let folder_meta = tree
            .get_meta(folder_node)
            .map_err(|e| IndexError::TreeOperation(format!("Failed to get folder meta: {}", e)))?;
        folder_meta
            .insert(TREE_META_NAME, new_name[0])
            .map_err(|e| {
                IndexError::TreeOperation(format!("Failed to update folder name: {}", e))
            })?;

        // Rewrite each descendant FILE node's denormalized `path` meta to its new
        // location. The tree is already re-parented, so `get_node_path` walks up
        // through the renamed folder and yields the correct new path — the same
        // derivation `rebuild_caches` uses, keeping the meta and the tree in lock-step.
        for descendant in self.descendant_file_nodes(folder_node) {
            let Some(new_path) = self.get_node_path(&descendant) else {
                continue;
            };
            let meta = tree.get_meta(descendant).map_err(|e| {
                IndexError::TreeOperation(format!("Failed to get descendant meta: {}", e))
            })?;
            meta.insert(TREE_META_PATH, new_path.as_str())
                .map_err(|e| {
                    IndexError::TreeOperation(format!(
                        "Failed to update descendant path meta: {}",
                        e
                    ))
                })?;
        }

        // Rebuild both caches + the deleted-paths guard from the now-consistent tree.
        self.rebuild_caches();

        tracing::info!(
            "Moved folder subtree in index: {} -> {}",
            old_prefix,
            new_prefix
        );
        Ok(())
    }

    /// Merge a loser folder node into a survivor folder node — the conflict resolver's
    /// folder-merge primitive (INV-1.5c), keyed by `TreeID` not by path.
    ///
    /// The resolver emits a `MergeFolder { survivor, loser }` only for two folder nodes
    /// at the SAME display path; this re-parents every ALIVE child of the loser under
    /// the survivor, then tombstones the now-emptied loser. Because both folders sit at
    /// the same display path, re-parenting a child from loser to survivor leaves that
    /// child's display path UNCHANGED (`P/child` either way) — so, unlike
    /// [`Self::move_subtree`], no descendant `path` meta needs rewriting; the caller's
    /// [`Self::rebuild_caches`] re-derives the (unchanged) paths from the tree.
    ///
    /// **Only ALIVE children move** (INV-3 / EC-7): a tombstoned child of the loser
    /// stays tombstoned and is not resurrected. Re-parenting all alive children OUT
    /// first is required before the delete — loro's `tree.delete` makes a deleted node's
    /// remaining children "not appear in the state", so any alive child left under the
    /// loser would be lost.
    ///
    /// Keyed by `TreeID` because folder-merge runs precisely when two folder nodes share
    /// one path, where a path-keyed lookup can't tell them apart. Same-name child
    /// collisions the union surfaces (two files at `P/Notes.md`, or two sub-folders at
    /// `P/sub`) are resolved by the resolver's other cases in the same plan — they are
    /// NOT this primitive's concern. In-memory only — the caller persists via
    /// [`Self::save_index`] and rebuilds caches.
    pub fn merge_folder_into(&self, survivor: TreeID, loser: TreeID) -> Result<()> {
        let tree = self.index_tree();

        // Defensive: a loser already tombstoned (or gone) is a no-op — the resolver
        // emits one merge per loser, so this should not happen, but skipping rather than
        // erroring keeps a redundant replay safe.
        if tree.is_node_deleted(&loser).unwrap_or(true) {
            tracing::debug!("merge_folder_into: loser {:?} already gone — no-op", loser);
            return Ok(());
        }

        // Re-parent every ALIVE direct child of the loser under the survivor. A
        // tombstoned child is left in the Deleted set (INV-3) — it must NOT move.
        let children = tree.children(loser).unwrap_or_default();
        for child in children {
            if tree.is_node_deleted(&child).unwrap_or(true) {
                continue;
            }
            tree.mov(child, TreeParentId::Node(survivor)).map_err(|e| {
                IndexError::TreeOperation(format!("Failed to re-parent folder child: {}", e))
            })?;
        }

        // Tombstone the emptied loser folder node.
        tree.delete(loser).map_err(|e| {
            IndexError::TreeOperation(format!("Failed to delete merged folder node: {}", e))
        })?;

        tracing::info!(
            "Merged folder node into survivor: {:?} <- {:?}",
            survivor,
            loser
        );
        Ok(())
    }

    /// Resolve a FOLDER node by its full prefix path (lookup-only — never creates).
    ///
    /// Folders are absent from `path_to_node` (only file nodes are cached), so this
    /// descends the prefix's segments from the root, requiring each to be an existing
    /// folder. Returns `None` if any segment is missing or is not a folder.
    fn find_folder_node(&self, prefix: &str) -> Option<TreeID> {
        let mut parent = TreeParentId::Root;
        let mut found = None;
        for segment in prefix.split('/') {
            let child = self.find_folder_child(&parent, segment)?;
            parent = TreeParentId::Node(child);
            found = Some(child);
        }
        found
    }

    /// Find an existing FOLDER child named `name` under `parent` (lookup-only).
    ///
    /// The non-creating half of [`Self::get_or_create_folder`], used to resolve a
    /// folder path without the create side-effect.
    fn find_folder_child(&self, parent: &TreeParentId, name: &str) -> Option<TreeID> {
        let tree = self.index_tree();
        let children = match parent {
            TreeParentId::Root => tree.roots(),
            TreeParentId::Node(parent_id) => tree.children(parent_id).unwrap_or_default(),
            _ => vec![],
        };
        for child_id in children {
            let Ok(meta) = tree.get_meta(child_id) else {
                continue;
            };
            let is_folder =
                Self::tree_meta_string(&meta, TREE_META_TYPE).as_deref() == Some("folder");
            let child_name = Self::tree_meta_string(&meta, TREE_META_NAME);
            if is_folder && child_name.as_deref() == Some(name) {
                return Some(child_id);
            }
        }
        None
    }

    /// Collect every alive FILE node beneath `folder_node` at any depth — the set
    /// whose denormalized `path` meta a subtree move must rewrite.
    ///
    /// Recurses the moved folder's `children`, descending into sub-folders and
    /// collecting file nodes. Folder nodes themselves are not collected (they carry no
    /// `path` meta to rewrite).
    fn descendant_file_nodes(&self, folder_node: TreeID) -> Vec<TreeID> {
        let tree = self.index_tree();
        let mut files = Vec::new();
        let mut stack = tree.children(folder_node).unwrap_or_default();
        while let Some(node_id) = stack.pop() {
            let Ok(meta) = tree.get_meta(node_id) else {
                continue;
            };
            match Self::tree_meta_string(&meta, TREE_META_TYPE).as_deref() {
                Some("file") => files.push(node_id),
                Some("folder") => stack.extend(tree.children(node_id).unwrap_or_default()),
                _ => {}
            }
        }
        files
    }

    /// Whether a path's node is deleted in the tree (or absent entirely).
    pub fn is_node_deleted(&self, path: &str) -> bool {
        match self.find_node_by_path(path) {
            Some(node_id) => {
                let tree = self.index_tree();
                tree.is_node_deleted(&node_id).unwrap_or(true)
            }
            None => true, // Not in tree = effectively deleted.
        }
    }

    /// Whether the deleted-paths resurrection guard currently covers `path`.
    ///
    /// The public read an inbound document-update flow consults before
    /// materializing a file: a path known-deleted in the index (set synchronously
    /// by `delete_node` and rederived on every `rebuild_caches`) must not be
    /// resurrected. Distinct from [`Self::is_node_deleted`], which queries live tree
    /// state for a path that still has a node.
    pub fn is_path_deleted(&self, path: &str) -> bool {
        self.sync_state.is_path_deleted_in_index(path)
    }

    /// Get or create a folder node under `parent` with the given `name`.
    fn get_or_create_folder(&self, parent: TreeParentId, name: &str) -> Result<TreeParentId> {
        let tree = self.index_tree();

        // Look for an existing folder with this name under the parent.
        let children = match &parent {
            TreeParentId::Root => tree.roots(),
            TreeParentId::Node(parent_id) => tree.children(parent_id).unwrap_or_default(),
            _ => vec![],
        };

        for child_id in children {
            if let Ok(meta) = tree.get_meta(child_id) {
                let is_folder =
                    Self::tree_meta_string(&meta, TREE_META_TYPE).as_deref() == Some("folder");
                let child_name = Self::tree_meta_string(&meta, TREE_META_NAME);

                if is_folder && child_name.as_deref() == Some(name) {
                    return Ok(TreeParentId::Node(child_id));
                }
            }
        }

        // Create a new folder node.
        let node_id = tree.create(parent).map_err(|e| {
            IndexError::TreeOperation(format!("Failed to create folder node: {}", e))
        })?;

        let meta = tree
            .get_meta(node_id)
            .map_err(|e| IndexError::TreeOperation(format!("Failed to get folder meta: {}", e)))?;
        meta.insert(TREE_META_TYPE, "folder")
            .map_err(|e| IndexError::TreeOperation(format!("Failed to set folder type: {}", e)))?;
        meta.insert(TREE_META_NAME, name)
            .map_err(|e| IndexError::TreeOperation(format!("Failed to set folder name: {}", e)))?;

        Ok(TreeParentId::Node(node_id))
    }
}
