//! Apply inbound Index CRDT updates: import the delta, rebuild the caches, detect
//! which paths the Index vacated on this device (deletes and moves), and clean up
//! the filesystem for them.
//!
//! Carried from `sync-core`'s `sync_engine/registry_apply.rs`. Two structural
//! departures the UUID model forces:
//!
//! - **The twin-dedupe machinery is gone.** It guarded a path-hash collision class
//!   (two alive nodes colliding on one `simple_hash(path)` doc_id) that UUID
//!   identity dissolves — distinct documents now have distinct UUIDs, so they never
//!   shadow each other in the caches. The inline duplicate-twin data-loss tests went
//!   with it.
//! - **`.loro` cleanup splits by reason.** A content `.loro` is addressed by UUID
//!   (`docs/<uuid>.loro`), path-independently. So a DELETED path's `.loro` must be
//!   removed (the document is gone), but a MOVED-away path's `.loro` must SURVIVE
//!   (the same document lives on at the new path under the same UUID). Conflating
//!   them — as the old path-addressed cleanup could — would delete a live document's
//!   content. The two sets are tracked and cleaned separately here.

use crate::content_doc::warn_if_pending;
use crate::fs::FileSystem;
use crate::vault::Vault;

use loro::TreeID;
use std::collections::HashMap;
use tracing::{debug, warn};

use super::{DocId, Result};

/// What an Index import vacated on this device, classified by reason so the caller
/// can both filter document updates and clean the filesystem correctly.
///
/// The distinction is load-bearing under UUID keying: a DELETED document's update
/// must be dropped (it would resurrect a tombstoned file), but a MOVED document's
/// update must still APPLY — it arrives under the same UUID, now valid at the new
/// path. So the caller filters document updates by [`Self::deleted_uuids`] only,
/// never by moved ones.
#[derive(Default)]
pub(super) struct VacatedPaths {
    /// `(path, uuid)` for nodes that were tombstoned (the document is gone). Both the
    /// `.md` and the `docs/<uuid>.loro` are removed, and the UUID's document update
    /// is filtered out (resurrection guard).
    deleted: Vec<(String, uuid::Uuid)>,
    /// Old paths a still-alive node moved away from. Only the old `.md` is removed —
    /// the `docs/<uuid>.loro` is path-independent and the node still owns it at its
    /// new path, so the moved document's update is NOT filtered.
    moved_from: Vec<String>,
}

impl VacatedPaths {
    /// The UUIDs of deleted documents, for filtering out document updates that would
    /// otherwise resurrect a tombstoned file. Moved documents are deliberately
    /// excluded — their updates apply at the new path.
    pub(super) fn deleted_uuids(&self) -> impl Iterator<Item = super::DocId> + '_ {
        self.deleted.iter().map(|(_, u)| super::DocId(*u))
    }
}

impl<F: FileSystem> Vault<F> {
    /// Apply Index updates from a sync response.
    ///
    /// Errors here `?`-propagate (whole-batch-fatal): an Index-delta corruption means
    /// we can't trust any of the batch, whereas a single corrupt document is
    /// per-item-recoverable (see `apply_doc_updates`).
    ///
    /// Imports the Index CRDT delta, rebuilds the caches, then cleans up the
    /// filesystem for paths the Index vacated on this device:
    ///
    /// - **Deletes** — a node whose path is now tombstoned. Detected from the
    ///   pre-rebuild cache (Loro doesn't expose a deleted node's parent, so its path
    ///   isn't walkable after rebuild). The `.md` and `docs/<uuid>.loro` are removed.
    /// - **Moves** — a node (same `TreeID`) that now lives at a different path than
    ///   before this import (a `tree.mov` on the sender). The old `.md` would
    ///   otherwise be stranded as an untracked orphan on every receiver; only it is
    ///   removed (the `.loro` is path-independent and survives).
    ///
    /// A vacated path that some alive node NOW occupies is excluded from cleanup
    /// (B1): in a swap (A leaves P1 while B moves into P1 in the same import),
    /// deleting B's freshly arrived file — and dropping its document update — would
    /// be permanent data loss.
    ///
    /// Returns the vacated-path classification so the caller can strip those paths
    /// from subsequent document updates (which would otherwise re-create the file on
    /// disk under the old path).
    pub(super) async fn apply_index_updates(&self, data: &[u8]) -> Result<VacatedPaths> {
        debug!("apply_index_updates: data_len={}", data.len());

        // Snapshot the pre-import path→node mapping so we can detect moves after the
        // cache is rebuilt. `TreeID` is `Copy`; cloning copies the map out of the
        // guard, a temporary dropped before `rebuild_caches` re-acquires the lock.
        let pre_import_paths: HashMap<String, TreeID> = self.index().path_to_node().clone();
        // Capture each pre-import path's document UUID too, so a vacated path's
        // content `.loro` can be located by UUID for cleanup (after rebuild a deleted
        // node is tombstoned and a moved node lives elsewhere, so neither path
        // resolves to its UUID through the rebuilt cache).
        let pre_import_uuids: HashMap<String, uuid::Uuid> = pre_import_paths
            .iter()
            .filter_map(|(path, id)| self.index().node_uuid(id).map(|u| (path.clone(), u)))
            .collect();

        // Import the Index delta (or snapshot — Loro merges either).
        let status = self.index().import_updates(data)?;

        // Surface an Index delta that arrived with unsatisfied causal deps (e.g. a
        // node-create op buffered pending because an ancestor folder-create is
        // missing). Logging only — the buffered ops apply when their deps land via a
        // later exchange, and boot reconciliation backstops anything still stranded.
        warn_if_pending(&status, "index");

        // Collect deleted paths from the cache BEFORE rebuilding it. After import the
        // tree marks deleted nodes internally, but `get_node_path` returns None for
        // them (Loro doesn't expose a deleted node's parent), so we check each cached
        // path against the tree while the pre-deletion mapping is still present.
        let deleted_paths: Vec<String> = {
            let tree = self.index().index_tree();
            self.index()
                .path_to_node()
                .iter()
                .filter(|(_, node_id)| tree.is_node_deleted(node_id).unwrap_or(false))
                .map(|(path, _)| path.clone())
                .collect()
        };

        // Rebuild the caches from the updated tree. This also re-derives the
        // deleted-paths guard from Index truth (reading each deleted node's `path`
        // meta) and applies "alive wins", so a peer's legitimate re-create at a
        // previously-deleted path is not blocked.
        self.index().rebuild_caches();

        // P2: fire cascade here over the merged state.
        //
        // A distinct-UUID same-path collision (two replicas independently created
        // different documents at the same path) surfaces in the rebuilt caches as a
        // path one node won and another lost. The conflict cascade (identical →
        // collapse / one-empty → non-empty wins / both-non-empty → conflict file)
        // runs here, over the freshly-merged Index + content state. Not implemented
        // in Phase 1 — under UUID keying a same-UUID merge is always a normal CRDT
        // merge, and the collision class is the cascade's job, handled here, never in
        // `apply_doc.rs`.

        // Alive-wins for the captured deleted set: `deleted_paths` was built from the
        // PRE-import cache, so a path some alive node still occupies after rebuild
        // must be dropped here — otherwise the fs cleanup below would physically
        // delete a live file (the re-create-after-delete / swap case). Pair each
        // surviving deleted path with the UUID it held pre-import (the tombstoned
        // node's UUID is no longer cache-resolvable).
        let deleted: Vec<(String, uuid::Uuid)> = {
            let rebuilt = self.index().path_to_node();
            deleted_paths
                .into_iter()
                .filter(|p| !rebuilt.contains_key(p.as_str()))
                .filter_map(|p| pre_import_uuids.get(&p).map(|u| (p, *u)))
                .collect()
        };

        // Detect moves: an old path whose node (same `TreeID`) now lives at a
        // different path. Orthogonal to the deleted set (which keys on tombstoned
        // nodes); a moved node stays alive, just under a new path.
        let mut moved_from: Vec<String> = Vec::new();
        {
            let rebuilt = self.index().path_to_node();
            let node_to_new_path: HashMap<TreeID, &String> =
                rebuilt.iter().map(|(p, id)| (*id, p)).collect();

            for (old_path, node_id) in &pre_import_paths {
                if let Some(new_path) = node_to_new_path.get(node_id)
                    && *new_path != old_path
                    // B1 exclusion: a vacated path an alive node now occupies must be
                    // neither fs-cleaned nor filtered from doc updates (the swap case).
                    && !rebuilt.contains_key(old_path)
                {
                    moved_from.push(old_path.clone());
                }
            }
        }

        let vacated = VacatedPaths { deleted, moved_from };

        // Clean up the filesystem for vacated paths (deletes remove `.md` + `.loro`,
        // moves remove only the old `.md`).
        self.cleanup_vacated_paths(&vacated).await?;

        // Persist the merged Index, then mark it synced so it reconciles before the
        // next sync import.
        self.index().save_index(self.fs()).await?;
        self.index().sync_state.mark_index_synced();

        debug!(
            "apply_index_updates: complete, deleted={:?}, moved_from={:?}",
            vacated.deleted, vacated.moved_from
        );
        Ok(vacated)
    }

    /// Filesystem cleanup for vacated paths.
    ///
    /// Deletes remove the `.md` and the `docs/<uuid>.loro` (the document is gone).
    /// Moves remove only the old `.md` — the `.loro` is UUID-addressed and the same
    /// document lives on at its new path. Both mark the path synced before deletion
    /// (echo detection) and drop the document cache entry under the vacated path.
    async fn cleanup_vacated_paths(&self, vacated: &VacatedPaths) -> Result<()> {
        for (path, uuid) in &vacated.deleted {
            self.remove_md_file(path).await;

            // Remove the content `.loro` by the UUID this path held (the document is
            // gone). A move would NOT reach here — its `.loro` is path-independent
            // and the node still owns it at the new path.
            let loro_path = Self::doc_content_path(&DocId(*uuid));
            if self.fs().exists(&loro_path).await.unwrap_or(false)
                && let Err(e) = self.fs().delete(&loro_path).await
            {
                warn!("Failed to delete .loro file {}: {}", loro_path, e);
            }

            self.documents_mut().remove(path);
        }

        for path in &vacated.moved_from {
            // Only the stale `.md` at the old path — the `.loro` survives (the same
            // document is now at its new path under the same UUID).
            self.remove_md_file(path).await;
            self.documents_mut().remove(path);
        }

        Ok(())
    }

    /// Remove a `.md` file at `path`, marking it synced first (echo detection).
    async fn remove_md_file(&self, path: &str) {
        if self.fs().exists(path).await.unwrap_or(false) {
            debug!("apply_index_updates: deleting {}", path);
            self.mark_synced(path);
            if let Err(e) = self.fs().delete(path).await {
                warn!("Failed to delete {}: {}", path, e);
            }
        }
    }
}
