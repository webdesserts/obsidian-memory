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
        let status = self.registry_mut().import(data).map_err(|e| {
            SyncEngineError::Deserialization(format!("Registry import failed: {}", e))
        })?;

        // Surface a registry delta that arrived with unsatisfied causal deps (e.g. a
        // node-create op buffered pending because an ancestor folder-create op is missing).
        // Logging only — the buffered ops apply when their deps land via a later exchange,
        // and boot reconciliation (C4) backstops anything still stranded.
        crate::document::warn_if_pending(&status, "registry");

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
            if self.fs.exists(&sync_path).await.unwrap_or(false)
                && let Err(e) = self.fs.delete(&sync_path).await
            {
                warn!("Failed to delete .loro file {}: {}", sync_path, e);
            }

            // Remove from documents cache
            self.documents_mut().remove(path);
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    //! Duplicate-twin / cross-peer-dedupe data-loss guards.
    //!
    //! These stay inline (rather than migrating to `tests/`) because they must
    //! control WHICH twin sits in the receiver's path cache when a tombstone
    //! arrives — the deleted-paths alive-guard only bites in the loser-cached
    //! ordering, and there is no public API to force a specific twin into the
    //! cache slot. Reaching `path_to_node_mut()` (`pub(crate)`) from in-crate
    //! tests is the legitimate exception to "test through the public API"; the
    //! alternative (widening the cache accessor to `pub`) would be testing an
    //! internal by exposing it, which the test principle forbids. The
    //! user-facing effect each asserts is "a dedupe/tombstone must never
    //! physically delete a file a live registry node still backs" (the Log.md /
    //! Working Memory.md mass-deletion class).

    use crate::PeerId;
    use crate::fs::{FileSystem, InMemoryFs};
    use crate::vault::Vault;

    use loro::TreeID;
    use std::sync::Arc;

    fn author(n: u8) -> PeerId {
        PeerId::from_bytes([n; 32])
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
        let recv_fs = Arc::new(InMemoryFs::new());
        let donor_fs = Arc::new(InMemoryFs::new());

        // Receiver registers note.md → twin R.
        recv_fs.write("note.md", b"# Note").await.unwrap();
        let recv = Vault::init(Arc::clone(&recv_fs), author(1)).await.unwrap();

        // A separate vault registers the SAME path independently → twin D (a
        // different TreeID, same path/doc_id). Importing donor's registry into the
        // receiver leaves BOTH twins alive at note.md — the duplicate-node pair.
        donor_fs.write("note.md", b"# Note").await.unwrap();
        let donor = Vault::init(Arc::clone(&donor_fs), author(2)).await.unwrap();
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
            !recv
                .file_tree()
                .is_node_deleted(&survivor_id)
                .unwrap_or(true),
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

    /// Which twin vault B has in its path cache when A's dedupe tombstone arrives.
    ///
    /// The deleted-paths alive-guard (`fc3a27e`) only matters when B has cached the LOSER:
    /// that is the ordering where the tombstone lands on the cached node and the path would
    /// (without the guard) enter the local deleted_paths vec and delete a still-occupied
    /// file. Testing the winner-cached case too proves the dedupe is safe in both orderings,
    /// and guards against a fixture that passes only by FxHashMap luck.
    #[derive(Clone, Copy, Debug)]
    enum CachedTwin {
        Winner,
        Loser,
    }

    /// Cross-peer dedupe-broadcast safety, parameterized on which twin B has cached.
    ///
    /// The scenario, built the production way:
    /// 1. Vault A holds a duplicate alive pair at `note.md` (two peers registered the path
    ///    independently, then one registry was imported into A).
    /// 2. A runs `find_registry_debris` + `apply_dedupe`, tombstoning the deterministic
    ///    LOSER (the higher TreeID) and keeping the winner.
    /// 3. Vault B ALSO holds both twins alive (it imported A's pre-dedupe registry) and has
    ///    the `.md` on disk. B's cache is arranged to hold `cached` (winner or loser).
    /// 4. B imports A's post-dedupe registry via the real `apply_registry_updates` path.
    ///
    /// Asserts B's `.md` survives, the winner is alive, the loser is tombstoned, and no fs
    /// deletion fired — proving the alive-guard protects the file when a dedup tombstone
    /// arrives at a path an alive winner still occupies.
    async fn cross_peer_dedupe_keeps_file(cached: CachedTwin) {
        // --- Build vault A with a duplicate alive pair at note.md (two peers). ---
        let fs_a = Arc::new(InMemoryFs::new());
        fs_a.write("note.md", b"# Note").await.unwrap();
        let vault_a = Vault::init(Arc::clone(&fs_a), author(1)).await.unwrap();

        let fs_other = Arc::new(InMemoryFs::new());
        fs_other.write("note.md", b"# Note").await.unwrap();
        let vault_other = Vault::init(Arc::clone(&fs_other), author(2)).await.unwrap();
        // Merge the other peer's registry into A → both twins alive at note.md.
        let other_snapshot = vault_other
            .registry()
            .export(loro::ExportMode::Snapshot)
            .unwrap();
        vault_a
            .apply_registry_updates(&other_snapshot)
            .await
            .unwrap();

        // Identify the two twins and the deterministic winner/loser from the report.
        let report = vault_a.find_registry_debris().await.unwrap();
        assert_eq!(
            report.duplicate_groups.len(),
            1,
            "vault A must see exactly one duplicate group at note.md"
        );
        let group = &report.duplicate_groups[0];
        let winner = group.winner;
        let loser = *group
            .alive_nodes
            .iter()
            .find(|id| **id != winner)
            .expect("the group has a loser twin");
        assert_eq!(
            winner,
            std::cmp::min(winner, loser),
            "winner is the min TreeID"
        );

        // --- Build vault B holding BOTH twins alive, with note.md on disk. ---
        // B imports A's PRE-dedupe registry, so B's two twins have the SAME TreeIDs A's do —
        // which is what makes A's later tombstone op (keyed on the loser's TreeID) land on a
        // node B actually has.
        let fs_b = Arc::new(InMemoryFs::new());
        fs_b.write("note.md", b"# Note").await.unwrap();
        let vault_b = Vault::init(Arc::clone(&fs_b), author(1)).await.unwrap();
        let a_pre_dedupe = vault_a
            .registry()
            .export(loro::ExportMode::Snapshot)
            .unwrap();
        vault_b.apply_registry_updates(&a_pre_dedupe).await.unwrap();
        assert!(
            fs_b.exists("note.md").await.unwrap(),
            "setup: note.md must be on B's disk before the dedupe tombstone arrives"
        );

        // Control which twin B has cached. rebuild_path_cache resolves the path to whichever
        // twin iterated last (FxHashMap order is deterministic-per-run), so an uncontrolled
        // fixture would only exercise one ordering by luck. Force the cache slot explicitly.
        let target = match cached {
            CachedTwin::Winner => winner,
            CachedTwin::Loser => loser,
        };
        vault_b
            .path_to_node_mut()
            .insert("note.md".to_string(), target);
        assert_eq!(
            *vault_b.path_to_node().get("note.md").unwrap(),
            target,
            "setup: B's cache must hold the {cached:?} twin"
        );

        // --- A runs the dedupe, then B imports A's post-dedupe registry. ---
        let stats = vault_a.apply_dedupe(&report).await.unwrap();
        assert_eq!(
            stats.nodes_tombstoned, 1,
            "A's dedupe tombstones exactly the loser"
        );
        let a_post_dedupe = vault_a
            .registry()
            .export(loro::ExportMode::Snapshot)
            .unwrap();
        vault_b
            .apply_registry_updates(&a_post_dedupe)
            .await
            .unwrap();

        // The winner stays alive, the loser is tombstoned, and — the load-bearing assertion —
        // B's .md survives because the winner still occupies the path. Without the alive-guard,
        // the Loser-cached ordering would have routed note.md into the local deleted_paths vec
        // and physically deleted a file that a live node still backs.
        assert!(
            !vault_b.file_tree().is_node_deleted(&winner).unwrap_or(true),
            "the winner twin must remain alive on B ({cached:?} cached)"
        );
        assert!(
            vault_b.file_tree().is_node_deleted(&loser).unwrap_or(false),
            "the loser twin must be tombstoned on B ({cached:?} cached)"
        );
        assert!(
            !vault_b.is_file_deleted("note.md"),
            "note.md must still resolve to the alive winner on B ({cached:?} cached)"
        );
        assert!(
            fs_b.exists("note.md").await.unwrap(),
            "note.md must NOT be physically deleted on B when an alive winner occupies the path ({cached:?} cached)"
        );
    }

    #[tokio::test]
    async fn test_cross_peer_dedupe_keeps_file_winner_cached() {
        // B cached the WINNER pre-import: the tombstone lands on the non-cached loser. The
        // file is safe here even without the alive-guard, but proving it keeps the dedupe
        // honest across both FxHashMap orderings.
        cross_peer_dedupe_keeps_file(CachedTwin::Winner).await;
    }

    #[tokio::test]
    async fn test_cross_peer_dedupe_keeps_file_loser_cached() {
        // B cached the LOSER pre-import: the tombstone lands on the cached node, which is the
        // exact case the deleted-paths alive-guard exists for. Only this ordering exercises
        // the fix; without it B would lose note.md.
        cross_peer_dedupe_keeps_file(CachedTwin::Loser).await;
    }
}
