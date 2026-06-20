use crate::document::NoteDocument;
use crate::fs::FileSystem;
use crate::vault::Vault;

use std::collections::HashMap;
use tracing::{debug, info, warn};

use super::Result;

impl<F: FileSystem> Vault<F> {
    /// Apply document updates from a sync response.
    ///
    /// Note: SyncResponse doesn't include mtime, so "latest wins" falls back to "remote wins"
    /// for initial sync. Real-time DocumentUpdate messages include mtime for proper resolution.
    pub(super) async fn apply_document_updates(
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
    pub(super) async fn apply_single_update(
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

            // Flow-2 apply gate: do NOT materialize a brand-new doc unless its registry
            // node is already present. Node presence is the RECEIVE-SIDE half of a
            // three-file contract; the other halves live in:
            //   - C3 send side (`prepare.rs`): when any new-doc snapshot ships, the full
            //     registry snapshot rides with it, so the node lands before this apply.
            //   - boot backstop (`reconcile.rs` C4): a pre-existing on-disk file lacking a
            //     node is adopted at load, healing anything this gate skipped.
            // The ordering this gate depends on is load-bearing: registry updates are
            // applied BEFORE document updates in `process.rs` (both the SyncResponse and
            // SyncExchange arms), so by the time we reach here the node from the same
            // message is already in `path_to_node`. Skipping (hard-skip, no disk write)
            // is correct: without a node, writing `.md`+`.loro` would mint exactly the
            // file-without-node divergence this fix exists to prevent. The only case this
            // ever fires is the bug case (node-create op absent though the doc arrived);
            // nothing is stranded on disk because the `.loro` was never written, and the
            // doc re-applies cleanly once its node arrives (C3) or is reconciled at boot (C4).
            //
            // One arm has NO preceding registry-apply: the real-time `DocumentUpdate`
            // case (`process.rs:155`) calls `apply_single_update` directly. Today that's
            // safe because a brand-new doc's node always reaches the peer via a SyncResponse/
            // SyncExchange (which DO apply registry first) before any real-time update for
            // it. If a future change starts broadcasting `DocumentUpdate`s for newly-created
            // docs over the native path, it MUST ship/establish the node first, or this gate
            // will hard-skip the create until a later registry sync or boot reconcile heals it.
            if !self.path_to_node().contains_key(path) {
                warn!(
                    "apply_single_update: skipping create for path with no registry node (Flow-2 gate): {}",
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
