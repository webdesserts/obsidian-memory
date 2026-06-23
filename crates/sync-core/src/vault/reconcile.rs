//! Reconcile: bring Loro documents and the registry back in sync with the
//! filesystem (markdown is the source of truth) on load and before sync imports.
//!
//! Extracted from `vault/mod.rs` as a sibling `impl<F: FileSystem> Vault<F>`
//! block — pure code-motion, no behavior change. Reaches the parent module's
//! `pub(crate)` items (`SYNC_DIR`, `TRASH_DIR`, `simple_hash`) and the
//! `pub(crate) sync_state` field via `super::`.

use super::{SYNC_DIR, TRASH_DIR, simple_hash};
use crate::document::NoteDocument;
use crate::fs::{FileSystem, FsError};
use crate::vault::{FileMove, ReconcileReport, Result, Vault, VaultError};
use std::collections::HashMap;

impl<F: FileSystem> Vault<F> {
    /// Reconcile filesystem state with Loro documents.
    ///
    /// This is called on load to handle changes made while the plugin was off:
    /// - External file additions → create new Loro docs
    /// - External file modifications → re-create Loro docs from markdown
    /// - External file moves → migrate Loro doc to new path hash
    /// - External file deletions → orphaned .loro files (logged, not deleted)
    /// - Tombstoned disk orphans → quarantined to `.trash/`
    ///
    /// The filesystem (markdown) is always the source of truth.
    ///
    /// Invariant for the orphan-quarantine A1 guard: callers must ensure the
    /// `path_to_node` cache is current before invoking. `Vault::load` guarantees
    /// this by running `rebuild_path_cache()` immediately before reconcile, and
    /// `register_file` updates the cache synchronously, so a re-created file is
    /// always visible. A manual re-trigger (e.g. the wasm plugin) after external
    /// changes that bypass `register_file` should `rebuild_path_cache()` first,
    /// otherwise the guard could read a stale cache and quarantine a live file.
    pub async fn reconcile(&self) -> Result<ReconcileReport> {
        let mut report = ReconcileReport::default();

        // Get all markdown files in the vault
        let md_files: std::collections::HashSet<String> =
            self.list_files().await?.into_iter().collect();

        // Get all .loro files in .sync/documents/
        let loro_hashes = self.list_loro_documents().await?;

        // Build mapping: path hash → path
        let path_to_hash: HashMap<String, String> = md_files
            .iter()
            .map(|path| (path.clone(), simple_hash(path)))
            .collect();
        let hash_to_path: HashMap<String, String> = path_to_hash
            .iter()
            .map(|(path, hash)| (hash.clone(), path.clone()))
            .collect();

        // Track which new files we've already matched to moved files
        let mut matched_new_files: std::collections::HashSet<String> =
            std::collections::HashSet::new();

        // First pass: identify orphaned .loro files and try to match them to new files
        let mut orphaned_docs: Vec<(String, NoteDocument)> = Vec::new();

        for hash in &loro_hashes {
            if !hash_to_path.contains_key(hash) {
                // This .loro has no matching markdown file - could be deleted or moved
                let sync_path = format!("{}/documents/{}.loro", SYNC_DIR, hash);
                if let Ok(bytes) = self.fs.read(&sync_path).await {
                    // Preserve the doc's stored META_PATH so move-matching and orphan
                    // reporting see the real deleted path (passing "" to from_bytes
                    // would clobber it). Peer ID is preserved either way.
                    if let Ok(doc) =
                        NoteDocument::from_bytes_preserve_path(&bytes, self.loro_author)
                    {
                        orphaned_docs.push((hash.clone(), doc));
                    }
                }
            }
        }

        // Collect new files (markdown exists but no .loro)
        let mut new_files: Vec<String> = Vec::new();
        for path in &md_files {
            let hash = simple_hash(path);
            if !loro_hashes.contains(&hash) {
                new_files.push(path.clone());
            }
        }

        // Try to match orphaned .loro files to new markdown files by content
        for (old_hash, orphaned_doc) in &orphaned_docs {
            let orphaned_content_hash = orphaned_doc.content_hash();
            // A legacy orphan with META_PATH="" yields Some("") here, which
            // unwrap_or_default would not catch — fall back to the hash so the move
            // log and report.moved.from carry a meaningful identifier, not "".
            let old_path = orphaned_doc
                .stored_path()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| old_hash.clone());

            for new_path in &new_files {
                if matched_new_files.contains(new_path) {
                    continue;
                }

                // Read new file content and compute hash
                if let Ok(bytes) = self.fs.read(new_path).await {
                    let content = String::from_utf8_lossy(&bytes);
                    if let Ok(new_doc) =
                        NoteDocument::from_markdown(new_path, &content, self.loro_author)
                        && new_doc.content_hash() == orphaned_content_hash
                    {
                        // Content matches - this is a move!
                        tracing::info!("File move detected: {} -> {}", old_path, new_path);

                        // Migrate the Loro doc to the new path
                        self.migrate_document(old_hash, new_path).await?;

                        report.moved.push(FileMove {
                            from: old_path.clone(),
                            to: new_path.clone(),
                        });
                        matched_new_files.insert(new_path.clone());
                        break;
                    }
                }
            }
        }

        // Process remaining markdown files.
        //
        // Each file is reconciled in isolation via `reconcile_one_file` and a
        // per-file error never aborts the whole pass. reconcile runs inside
        // Vault::load, so propagating a per-file fs error here would abort daemon
        // startup over a single file (e.g. one race-deleted between the directory
        // scan and this loop) — log-and-continue, mirroring `index_existing_files`
        // and the orphan-quarantine branch. NotFound (a vanished file) is benign and
        // debug-logged; other errors warn.
        for path in &md_files {
            if matched_new_files.contains(path) {
                // Already handled as a move target
                continue;
            }

            match self
                .reconcile_one_file(path, &loro_hashes, &mut report)
                .await
            {
                Ok(()) => {}
                Err(VaultError::Fs(FsError::NotFound(_))) => {
                    tracing::debug!("Skipping race-deleted file during reconcile: {}", path);
                }
                Err(e) => {
                    tracing::warn!("Failed to reconcile {}: {}", path, e);
                }
            }
        }

        // Report orphaned .loro files that weren't matched to moves
        for (hash, doc) in &orphaned_docs {
            // Legacy orphans persisted before `5fd4a63` may carry META_PATH="", so
            // stored_path() returns Some("") and won't fall back on its own. Filter
            // the empty string so the warning + report carry the hash, not "".
            let old_path = doc
                .stored_path()
                .filter(|s| !s.is_empty())
                .unwrap_or_else(|| hash.clone());
            let was_moved = report.moved.iter().any(|m| m.from == old_path);
            if !was_moved {
                tracing::warn!("Orphaned .loro file (deleted?): {}", old_path);
                report.orphaned.push(old_path);
            }
        }

        // Inverse divergence: alive registry nodes whose backing `.md` is gone from disk.
        // REPORT-ONLY — see report_missing_files: never recreate the file or tombstone
        // the node here (the resurrection/deletion data-loss classes).
        self.report_missing_files(&md_files, &mut report).await;

        // Persist the registry mutations made during this reconcile pass. Batched here
        // (not per on_file_changed/register_file call) to avoid O(n) snapshot writes when
        // hundreds of files are indexed at startup. `adopted` must be included: an adopt
        // registers a node that only lives in memory until saved — without this the heal
        // is illusory (the node re-adopts on every restart, never persisting). Moves
        // aren't included: each move goes through migrate_document, which already persists
        // its own registration.
        if !report.indexed.is_empty() || !report.adopted.is_empty() {
            self.save_registry().await?;
        }

        Ok(report)
    }

    /// Record alive registry file nodes that have no backing `.md` on disk
    /// (`report.missing_files`) — the inverse of the adopt direction.
    ///
    /// This is the resurrection/deletion-risk direction and is deliberately REPORT-ONLY:
    /// reconcile does NOT recreate the file from the node's `.loro` (resurrection) and
    /// does NOT tombstone the node (deletion-propagation) — both are the data-loss classes
    /// the registry-truth resurrection guard hardened against. The both-gone subset
    /// (no `.md` AND no `.loro`) is also surfaced by the `find_registry_debris` operator
    /// tool as a "relic"; this surface is broader (it reports a missing `.md` even when the
    /// `.loro` is still present) and never mutates, so the two do not fight.
    ///
    /// `md_files` is the same on-disk markdown set `reconcile` already scanned, passed in
    /// so this pass needs no second directory walk.
    async fn report_missing_files(
        &self,
        md_files: &std::collections::HashSet<String>,
        report: &mut ReconcileReport,
    ) {
        // Collect alive file-node paths while holding the tree borrow, then release it
        // before the fs probes — mirror find_registry_debris (never hold a registry guard
        // across an await). path_to_node already holds exactly the alive file nodes
        // (rebuild_path_cache populates it from alive `type=="file"` nodes), so it is the
        // cheap source of truth here — no tree walk needed.
        let alive_paths: Vec<String> = self.path_to_node().keys().cloned().collect();

        for path in alive_paths {
            // A path whose `.md` is on disk is consistent; only a missing `.md` is the
            // inverse divergence. Use the already-scanned md_files set first to avoid an
            // fs probe for the common (present) case.
            if md_files.contains(&path) {
                continue;
            }
            if !self.fs.exists(&path).await.unwrap_or(false) {
                tracing::info!("Registry node with no backing file (report-only): {}", path);
                report.missing_files.push(path);
            }
        }
    }

    /// Reconcile a single markdown file against its Loro state, recording the
    /// outcome in `report`.
    ///
    /// Extracted from `reconcile`'s per-file loop so the caller can apply one
    /// log-and-continue handler around every branch (a file race-deleted mid-scan
    /// must not abort `Vault::load`). The quarantine branch keeps its own inner
    /// log-and-continue so a quarantine failure surfaces its specific message and
    /// is never mistaken for a generic reconcile error by the caller.
    async fn reconcile_one_file(
        &self,
        path: &str,
        loro_hashes: &std::collections::HashSet<String>,
        report: &mut ReconcileReport,
    ) -> Result<()> {
        let hash = simple_hash(path);
        let sync_path = self.document_sync_path(path);

        // Classify the file by the three axes the reconcile decision actually turns on,
        // then `match` over the tuple so every case is enumerable and the dangerous ones
        // fall out of exhaustiveness rather than a hand-written guard. The prior nested
        // `if loro_hashes.contains / else if tombstoned / else` keyed on a *mix* of
        // `.loro`-presence and tombstone state on a single axis, which hid that a
        // tombstoned-but-`.loro`-present path took the reindex arm — the S1 resurrection
        // risk. Making node-presence explicit is also what lets boot reconcile adopt a
        // `.loro` that has no node (the fs↔loro divergence heal).
        //
        // Axes:
        // - has_loro:   a `.loro` document file exists on disk for this path.
        // - has_node:   an *alive* registry tree node exists for this path.
        // - tombstoned: the registry tree marks this path deleted (alive-wins: a path
        //               with an alive node is never tombstoned after rebuild_path_cache).
        let has_loro = loro_hashes.contains(&hash);
        let has_node = self.path_to_node().contains_key(path);
        let tombstoned = self.is_path_deleted_in_registry(path);

        match (has_loro, has_node, tombstoned) {
            // `.loro` + node both present → both exist: check if markdown was modified
            // externally and re-index if so (unchanged behavior). The `_` on tombstoned is
            // correct: alive-wins means a live-node path is not tombstoned, but even in the
            // stale-armed window (delete_file inserted before rebuild) an alive node makes
            // this the reindex case, never a resurrection.
            (true, true, _) => {
                if self.needs_reindex(path, &sync_path).await? {
                    tracing::info!("File modified externally, re-indexing: {}", path);
                    self.reindex_file(path).await?;
                    report.reindexed.push(path.to_string());
                }
            }

            // `.loro` present, NO node, NOT tombstoned → the fs↔loro divergence: a peer's
            // content landed on disk without its registry node. ADOPT the existing `.loro`
            // by registering a node for it. register_file writes the node + path-derived
            // doc_id meta WITHOUT touching the `.loro`, so the document's own lineage
            // doc_id is preserved. Rebuilding from `.md` instead would mint a fresh lineage
            // doc_id → on the next sync the file would look independently-created and
            // diverge ("latest wins") instead of CRDT-merging. Adopt-not-rebuild is
            // mandatory. register_file is sync (no await) and no-ops if a node already
            // exists, so it cannot fight a concurrent re-create.
            (true, false, false) => {
                tracing::info!("Adopting orphaned .loro (no registry node): {}", path);
                self.register_file(path)?;
                report.adopted.push(path.to_string());
            }

            // Tombstoned (with or without `.loro`) → quarantine; NEVER resurrect. The
            // S1 case `(true, false, true)` — a `.loro`-present path the registry has
            // tombstoned — lands HERE by exhaustiveness, so a real user deletion is not
            // resurrected as an adopted node. A disk `.md` whose registry state is
            // tombstoned is an untracked orphan (a historical-bug or offline-window
            // strand), not a new file: move it to `.trash/` rather than re-minting a
            // node. The `.trash/` exclusion in list_files (and the watcher) keeps this
            // idempotent — the moved file is no longer a reconcile candidate.
            // quarantine_orphan's own A1 guard short-circuits (no quarantine) if an alive
            // node occupies the path, so the stale-armed `(false, true, true)` window is
            // safe here too.
            //
            // Inner log-and-continue (mirroring index_existing_files): cleanup of one
            // orphan failing must not block the vault from loading, and its specific
            // failure message is more useful than the caller's generic one.
            (_, _, true) => {
                if let Err(e) = self.quarantine_orphan(path).await {
                    tracing::warn!("Failed to quarantine orphan {}: {}", path, e);
                } else {
                    report.quarantined.push(path.to_string());
                }
            }

            // No `.loro`, not tombstoned → treat as a (possibly already-noded) new file.
            // on_file_changed registers the file node in the tree as part of creating its
            // document, so no separate register_file call is needed; register_file no-ops
            // if a node is already present, so the degenerate `(false, true, false)` case
            // (node but no `.loro`, e.g. a `.loro` deleted out from under a live node) is
            // handled the same way — recreate the `.loro` from markdown.
            (false, _, false) => {
                tracing::info!("New file detected, indexing: {}", path);
                self.on_file_changed(path).await?;
                report.indexed.push(path.to_string());
            }
        }

        Ok(())
    }

    /// Move a tombstoned disk orphan to `.trash/<path>`.
    ///
    /// A disk orphan is a `.md` file on disk whose registry state is tombstoned
    /// (`is_path_deleted_in_registry`). Quarantining is reversible (the file is
    /// preserved under `.trash/`, which `list_files` and the watcher exclude) and
    /// touches only disk — never the registry tree — so it cannot fight the
    /// resurrection guard or `delete_file`, which read the same `deleted_paths` set.
    ///
    /// A1 guard: if a live node currently occupies the path, the file is NOT an
    /// orphan no matter how `deleted_paths` looks. `delete_file` inserts into
    /// `deleted_paths` synchronously and `register_file` on a local re-create does
    /// NOT clear it, so a path can be stale-tombstoned while carrying a freshly
    /// re-created live node. Without this short-circuit, a reconcile triggered in
    /// that window (e.g. the wasm plugin's manual trigger) would delete the user's
    /// recreated file. The alive-node check closes that data-loss path in every
    /// calling context.
    ///
    /// `pub(crate)` (not module-private) so the inline vault tests, now a sibling
    /// module after the reconcile split, can drive the quarantine path directly.
    pub(crate) async fn quarantine_orphan(&self, path: &str) -> Result<()> {
        // A1: an alive node at the path means this is not an orphan — never quarantine.
        if self.path_to_node().contains_key(path) {
            return Ok(());
        }

        let bytes = self.fs.read(path).await?;

        // Pick the trash destination. Crash-idempotency: if a prior quarantine wrote
        // `.trash/<path>` but failed to delete the original (the partial-failure
        // window below), the orphan sits at BOTH paths. On the next pass we must reuse
        // that identical trash copy and just retry the delete — NOT allocate a new
        // collision suffix, which would let `.trash/<path>.N` grow without bound under
        // a persistent delete failure. A new suffix is only for a genuinely distinct
        // orphan that happens to share a name (different content).
        let base_dest = format!("{}/{}", TRASH_DIR, path);
        let dest = match self.fs.read(&base_dest).await {
            // Already-trashed identical content → reuse it, skip the write, retry delete.
            Ok(existing) if existing == bytes => base_dest,
            // Occupied by different content → suffix to avoid clobbering a distinct file.
            Ok(_) => {
                let mut n = 1;
                loop {
                    let candidate = format!("{}/{}.{}", TRASH_DIR, path, n);
                    match self.fs.read(&candidate).await {
                        Ok(existing) if existing == bytes => break candidate,
                        Ok(_) => n += 1,
                        Err(_) => break candidate,
                    }
                }
            }
            // Nothing there yet → use the base destination.
            Err(_) => base_dest,
        };

        // Write only when the destination doesn't already hold our content (the
        // crash-idempotency reuse case above skips it). NativeFs has no move primitive,
        // so mirror migrate_document: atomic_write (write-temp + rename) keeps the
        // trash copy from being torn if the process crashes mid-write, then delete the
        // original. `write` creates parent dirs (FileSystem trait contract), so `.trash/`
        // is created on demand.
        if !self.fs.exists(&dest).await? {
            self.fs.atomic_write(&dest, &bytes).await?;
        }

        // The write→delete sequence is non-atomic: if delete fails here, the copy is
        // safely in trash but the original remains. Surface that distinct partial state
        // so the next pass's idempotent reuse (above) is the recovery, not data loss.
        if let Err(e) = self.fs.delete(path).await {
            tracing::warn!(
                "Quarantine partially succeeded for {}: copy is in {} but the original \
                 could not be removed ({}); it will be retried on the next reconcile",
                path,
                dest,
                e
            );
            return Err(e.into());
        }

        tracing::info!("Quarantined disk orphan {} -> {}", path, dest);
        Ok(())
    }

    /// Migrate a Loro document from old path hash to new path.
    ///
    /// This preserves the CRDT history when a file is moved/renamed.
    /// Uses `from_bytes` to import before setting metadata, preserving the original peer ID.
    async fn migrate_document(&self, old_hash: &str, new_path: &str) -> Result<()> {
        let old_sync_path = format!("{}/documents/{}.loro", SYNC_DIR, old_hash);
        let new_hash = simple_hash(new_path);
        let new_sync_path = format!("{}/documents/{}.loro", SYNC_DIR, new_hash);

        // Load the old document (import first, then update path - preserves peer ID)
        let bytes = self.fs.read(&old_sync_path).await?;
        let doc = NoteDocument::from_bytes(new_path, &bytes, self.loro_author)?;

        // Save to new location
        let snapshot = doc.export_snapshot()?;
        self.fs.atomic_write(&new_sync_path, &snapshot).await?;

        // Delete old file
        self.fs.delete(&old_sync_path).await?;

        // Update cache
        self.documents_mut().insert(new_path.to_string(), doc);

        // Register in tree (the old path's node was already processed as orphaned)
        self.register_file(new_path)?;
        self.save_registry().await?;

        Ok(())
    }

    /// List all .loro document hashes in .sync/documents/
    async fn list_loro_documents(&self) -> Result<std::collections::HashSet<String>> {
        let mut hashes = std::collections::HashSet::new();
        let docs_dir = format!("{}/documents", SYNC_DIR);

        if !self.fs.exists(&docs_dir).await? {
            return Ok(hashes);
        }

        let entries = self.fs.list(&docs_dir).await?;
        for entry in entries {
            if !entry.is_dir && entry.name.ends_with(".loro") {
                // Extract hash from filename (remove .loro extension)
                let hash = entry.name.trim_end_matches(".loro").to_string();
                hashes.insert(hash);
            }
        }

        Ok(hashes)
    }

    /// Check if a file needs re-indexing (markdown content differs from Loro state)
    async fn needs_reindex(&self, md_path: &str, loro_path: &str) -> Result<bool> {
        // Read markdown content
        let md_bytes = self.fs.read(md_path).await?;
        let md_content = String::from_utf8_lossy(&md_bytes);

        // Load Loro doc and convert to markdown
        let loro_bytes = self.fs.read(loro_path).await?;
        let doc = match NoteDocument::from_bytes(md_path, &loro_bytes, self.loro_author) {
            Ok(d) => d,
            Err(_) => return Ok(true), // Corrupted Loro doc - needs reindex
        };
        let loro_content = doc.to_markdown();

        // Compare (normalize line endings)
        let md_normalized = md_content.replace("\r\n", "\n");
        let loro_normalized = loro_content.replace("\r\n", "\n");

        Ok(md_normalized != loro_normalized)
    }

    /// Re-index a file by diff-merging changes into the existing Loro doc.
    ///
    /// This is used when external modifications are detected during reconciliation.
    /// Preserves the peer ID by updating the existing document rather than replacing it.
    async fn reindex_file(&self, path: &str) -> Result<()> {
        let bytes = self.fs.read(path).await?;
        let content = String::from_utf8_lossy(&bytes);
        let parsed = crate::markdown::parse(&content);

        // Load existing .loro document
        let sync_path = self.document_sync_path(path);
        let loro_bytes = self.fs.read(&sync_path).await?;
        let doc = NoteDocument::from_bytes(path, &loro_bytes, self.loro_author)?;

        // Diff-merge the changes (preserves peer ID)
        let body_changed = doc.update_body(&parsed.body)?;
        let fm_changed = doc.update_frontmatter(parsed.frontmatter.as_ref())?;

        if body_changed || fm_changed {
            doc.commit();
            let snapshot = doc.export_snapshot()?;
            self.fs.atomic_write(&sync_path, &snapshot).await?;
            tracing::debug!("Re-indexed document via diff: {}", path);
        }

        // Update cache
        self.documents_mut().insert(path.to_string(), doc);

        Ok(())
    }

    // ========== Sync Consistency Methods ==========

    /// Ensure consistency of all pending paths before processing sync messages.
    ///
    /// Called at the start of `process_sync_message()` to guarantee that loro documents
    /// and the registry match the filesystem before importing sync data. This prevents
    /// panics when sync operations reference positions that don't exist in stale documents.
    pub(crate) async fn ensure_consistency(&self) -> Result<()> {
        // Reconcile registry first (file tree must be consistent before documents)
        if self.sync_state.take_registry_pending() {
            self.reconcile_registry().await?;
        }

        // Reconcile pending document paths
        let pending = self.sync_state.take_pending_reconcile();
        for path in pending {
            match self.reconcile_single(&path).await {
                Ok(true) => tracing::debug!("Reconciled stale doc before sync: {}", path),
                Ok(false) => {} // Already consistent
                Err(e) => {
                    // Check if file was deleted - skip gracefully
                    if matches!(e, VaultError::Fs(ref fs_err) if matches!(fs_err, crate::fs::FsError::NotFound(_)))
                    {
                        tracing::debug!("Skipping deleted file during reconcile: {}", path);
                    } else {
                        return Err(e);
                    }
                }
            }
        }
        Ok(())
    }

    /// Reconcile a single document by updating loro to match filesystem.
    ///
    /// Returns Ok(true) if reconciled (content differed), Ok(false) if already consistent.
    /// Returns Err with FsError::NotFound if the file was deleted.
    async fn reconcile_single(&self, path: &str) -> Result<bool> {
        let md_bytes = self.fs.read(path).await?; // May return NotFound
        let md_content = String::from_utf8_lossy(&md_bytes);

        let sync_path = self.document_sync_path(path);
        let loro_bytes = match self.fs.read(&sync_path).await {
            Ok(bytes) => bytes,
            Err(_) => return Ok(false), // No loro file yet, nothing to reconcile
        };

        let doc = match NoteDocument::from_bytes(path, &loro_bytes, self.loro_author) {
            Ok(d) => d,
            Err(_) => return Ok(false), // Corrupted loro doc, let sync recreate it
        };

        let loro_content = doc.to_markdown();

        // Normalize and compare
        let md_normalized = md_content.replace("\r\n", "\n");
        let loro_normalized = loro_content.replace("\r\n", "\n");

        if md_normalized == loro_normalized {
            return Ok(false); // Already consistent
        }

        // Update loro to match filesystem
        let parsed = crate::markdown::parse(&md_content);
        let body_changed = doc.update_body(&parsed.body)?;
        let fm_changed = doc.update_frontmatter(parsed.frontmatter.as_ref())?;

        if body_changed || fm_changed {
            doc.commit();
            let snapshot = doc.export_snapshot()?;
            self.fs.atomic_write(&sync_path, &snapshot).await?;
        }

        self.documents_mut().insert(path.to_string(), doc);
        Ok(true)
    }

    /// Reconcile registry by reloading from disk.
    ///
    /// Ensures the in-memory registry matches the persisted state before sync import.
    async fn reconcile_registry(&self) -> Result<()> {
        let registry_path = format!("{}/registry.loro", SYNC_DIR);
        if let Ok(data) = self.fs.read(&registry_path).await {
            self.registry_mut().import(&data).map_err(|e| {
                VaultError::RegistryImport(format!("Registry reconcile failed: {}", e))
            })?;
            self.rebuild_path_cache();
            tracing::debug!("Reconciled registry before sync");
        }
        Ok(())
    }
}
