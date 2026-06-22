//! Boot reconciliation: bring the Index back in sync with the filesystem before
//! the vault opens to remote sync (INV-7, FR-7).
//!
//! Carried from `sync-core`'s `vault/reconcile.rs`, re-keyed on UUID. Despite living
//! under `index/`, this is an `impl<F: FileSystem> Vault<F>` block: reconcile needs
//! the filesystem, the document cache, and the Index together — all of which the
//! public [`Vault`] handle owns. The Index alone is fs-agnostic.
//!
//! ## What reconcile owns (the fs↔loro divergence heal)
//!
//! The sync engine is purely event-driven, so any gap between disk and the Index is
//! permanent without a reconciliation pass. On every load `reconcile`:
//!
//! - **Adopts** an orphaned content doc (`docs/<uuid>.loro` with no live node) by
//!   registering a node for it, healing the divergence class where a peer's content
//!   landed on disk without its Index node. Under UUID keying the content doc is
//!   location-agnostic (OQ-3 deleted its stored `_meta.path`), so the orphan is
//!   paired to its on-disk `.md` by content hash, and the adopted node's path is the
//!   `.md`'s path.
//! - **Re-attaches a native move's lineage** (EC-8/S1): a `delete(old)` + `create(new)`
//!   on disk leaves the old node tombstoned (carrying `path == old`, `uuid == U`) and
//!   `docs/U.loro` orphaned. The new `.md`, when its content matches the orphan and
//!   the orphan's tombstoned path differs from the new path, re-attaches U at the new
//!   path — a zero-content move (the content `.loro` is never rewritten).
//! - **Quarantines** a tombstoned disk orphan (a `.md` still on disk at a tombstoned
//!   path) to `.trash/` — NEVER resurrecting a user's deletion.
//! - **Reports** an alive node whose backing `.md` is gone (REPORT-ONLY: it neither
//!   recreates the file nor tombstones the node — both are data-loss classes).
//! - **Repairs** a stale denormalized `content_version` (S3): a crash between a
//!   content commit and the meta update can leave the fingerprint stale; reconcile
//!   recomputes it from the content doc's actual `state_vv()` so the compare digest
//!   (P3) can be trusted.
//!
//! ## Boot order (INV-7 — load-bearing)
//!
//! `Vault::load` loads the Index (hard-failing on corruption — EC-9), rebuilds the
//! caches, runs `reconcile` (documenting local fs into the Index), and ONLY THEN
//! opens to remote (`process_message`). Local state is fully captured before any
//! remote delta integrates — "commit before pull."

use crate::content_doc::ContentDoc;
use crate::fs::{FileSystem, FsError};
use crate::hash::{content_hash, content_version_fingerprint};
use crate::index::{
    DOCS_DIR, FileMove, IndexError, ReconcileReport, Result, TRASH_DIR, content_doc_path,
};
use crate::vault::Vault;

use std::collections::HashSet;
use tracing::{debug, info, warn};
use uuid::Uuid;

impl<F: FileSystem> Vault<F> {
    /// Reconcile the filesystem with the Index — the boot pass that documents local
    /// fs state into the catalog before the vault opens to remote sync.
    ///
    /// The filesystem (the materialized `.md`) is the source of truth for local
    /// state. The pass is structured so the dangerous outcomes (resurrecting a
    /// deletion, deleting a live file) cannot happen:
    /// - orphaned content docs are adopted or their move-lineage re-attached, never
    ///   silently dropped (INV-3);
    /// - tombstoned disk orphans are quarantined, never resurrected;
    /// - alive nodes with no `.md` are reported only, never recreated or tombstoned.
    ///
    /// Per-file failures never abort the pass (a file race-deleted mid-scan must not
    /// fail `Vault::load`); a corrupt single content doc is contained and skipped
    /// (NFR-6). The Index is persisted once at the end if anything changed.
    pub async fn reconcile(&self) -> Result<ReconcileReport> {
        let mut report = ReconcileReport::default();

        // Every markdown file on disk, and every document UUID with a content `.loro`.
        let md_files: HashSet<String> = self.list_files().await?.into_iter().collect();
        let doc_uuids = self.list_content_docs().await?;

        // First pass: adopt orphaned content docs (no live node) by content-pairing
        // them to new `.md` files — native-move lineage re-attach or divergence heal.
        // Returns the `.md` paths it matched so the per-file loop skips them.
        let matched = self
            .adopt_orphans(&md_files, &doc_uuids, &mut report)
            .await?;

        // Second pass: reconcile each remaining `.md` against its Index/content state.
        //
        // Each file is reconciled in isolation and a per-file error never aborts the
        // pass: reconcile runs inside `Vault::load`, so propagating a per-file fs error
        // would abort startup over a single file (e.g. one race-deleted between the
        // directory scan and this loop). NotFound (a vanished file) is benign and
        // debug-logged; other errors warn.
        for path in &md_files {
            if matched.contains(path) {
                continue;
            }
            match self.reconcile_one_file(path, &mut report).await {
                Ok(()) => {}
                Err(IndexError::Fs(FsError::NotFound(_))) => {
                    debug!("Skipping race-deleted file during reconcile: {}", path);
                }
                Err(e) => {
                    warn!("Failed to reconcile {}: {}", path, e);
                }
            }
        }

        // Inverse divergence: alive Index nodes whose backing `.md` is gone from disk.
        // REPORT-ONLY — never recreate the file (resurrection) or tombstone the node
        // (deletion-propagation), both data-loss classes.
        self.report_missing_files(&md_files, &mut report).await;

        // S3: repair any stale denormalized `content_version` against the content
        // doc's actual `state_vv()`, so the compare digest (P3) reads a correct cache.
        let repaired = self.repair_content_versions().await?;

        // Recover any folder-swept orphan (EC-7/OQ-6) that persisted across this load: a
        // concurrent add a peer's folder delete tombstoned, whose own parent is still a
        // dead folder node. The apply path rescues these inline, but boot reconcile is the
        // backstop for one that survived a restart (e.g. the apply was interrupted before
        // the rescue, or the orphan's content arrived only after the apply that swept it).
        // It revives + re-homes + re-materializes + persists internally when it acts.
        self.rescue_swept_orphans().await?;

        // Materialize the folder set from the Index (INV-1.5a): a fresh clone / `reload`
        // re-creates each tracked empty directory and removes a tombstoned folder's empty
        // directory. Folders are invisible to the file passes above (which see only
        // `.md` files), so without this an empty folder never appears on a freshly-loaded
        // vault. fs-only (no Index mutation), so it runs regardless of the save gate.
        self.materialize_folders().await?;

        // Persist the Index mutations made during this pass — batched here (not per
        // adopt/register) to avoid O(n) snapshot writes when many files are indexed at
        // startup. `adopted`/`moved` register nodes that only live in memory until
        // saved; a `content_version` repair likewise mutates node meta. Without the
        // save the heal is illusory (it re-runs on every restart, never persisting).
        if report.has_changes() || repaired {
            self.save_index().await?;
        }

        Ok(report)
    }

    /// Adopt orphaned content docs — the fs↔loro divergence heal and the native-move
    /// lineage re-attach. Returns the set of `.md` paths that were adopted (matched to
    /// an orphan) so the per-file loop skips them.
    ///
    /// An orphan is a `docs/<uuid>.loro` whose UUID has no *live* node. Each orphan is
    /// content-paired (the only link, since OQ-3 removed the content doc's stored path)
    /// to an unmatched new `.md`:
    /// - If the orphan's UUID has a **tombstoned node** at some `old` (the move's
    ///   source), it re-attaches at a content-matching new `.md` whose path differs
    ///   from `old` (a move — a same-path `.md` at `old` is a strand, left for the
    ///   per-file loop to quarantine). Zero content re-transfers: the content `.loro`
    ///   is never rewritten, so a synced peer already holds it (S1, INV-1).
    /// - Otherwise (no node ever — pure divergence) it adopts at any content-matching
    ///   new `.md`, preserving the orphan's lineage UUID rather than minting (a rebuild
    ///   from `.md` would mint a fresh UUID and look independently-created on the next
    ///   sync, diverging instead of merging).
    ///
    /// "Window bound" for the move case is structural: only an *orphaned* `.loro`
    /// (UUID with no live node) is a candidate, so a new `.md` whose content merely
    /// coincides with a still-live document is never adopted onto that document — it
    /// is a fresh file with a fresh UUID.
    async fn adopt_orphans(
        &self,
        md_files: &HashSet<String>,
        doc_uuids: &HashSet<Uuid>,
        report: &mut ReconcileReport,
    ) -> Result<HashSet<String>> {
        let mut matched: HashSet<String> = HashSet::new();

        // Orphans: a content `.loro` on disk whose UUID has no live node.
        let orphans: Vec<Uuid> = doc_uuids
            .iter()
            .filter(|uuid| self.index().find_node_by_uuid(uuid).is_none())
            .copied()
            .collect();
        if orphans.is_empty() {
            return Ok(matched);
        }

        // New `.md` files (no live node) with their content hashes, computed once.
        // A bad file is skipped (it cannot be a move/divergence target if unreadable).
        let mut new_files: Vec<(String, [u8; 32])> = Vec::new();
        for path in md_files {
            if self.index().node_for_path(path).is_some() {
                continue;
            }
            if let Ok(bytes) = self.fs().read(path).await {
                let content = String::from_utf8_lossy(&bytes);
                if let Ok(doc) = ContentDoc::from_markdown(&content, self.loro_author()) {
                    new_files.push((path.clone(), content_hash(&doc)));
                }
            }
        }

        for uuid in orphans {
            let loro_path = content_doc_path(&uuid);
            // A corrupt single content doc is contained: skip it and keep going
            // (NFR-6 — one bad doc never aborts reconcile or drops the rest).
            let Ok(bytes) = self.fs().read(&loro_path).await else {
                continue;
            };
            let Ok(orphan_doc) = ContentDoc::from_bytes(&bytes, self.loro_author()) else {
                warn!("Skipping corrupt orphaned content doc: {}", loro_path);
                continue;
            };
            let orphan_hash = content_hash(&orphan_doc);

            // The orphan's last-known path, recovered from a tombstoned node (OQ-3).
            // `Some(old)` ⇒ a delete happened (the native-move source); `None` ⇒ the
            // UUID never had a node (pure fs↔loro divergence).
            let old_path = self.index().deleted_node_path_for_uuid(&uuid);

            // Pick a content-matching, unmatched new `.md`. For a move source, exclude
            // the orphan's own tombstoned path: a `.md` sitting AT the tombstoned path
            // is a strand to quarantine, not a move target (adopting it there would
            // resurrect the deletion).
            let candidate = match &old_path {
                Some(old) => new_files
                    .iter()
                    .find(|(p, h)| *h == orphan_hash && p != old && !matched.contains(p)),
                None => new_files
                    .iter()
                    .find(|(p, h)| *h == orphan_hash && !matched.contains(p)),
            };

            let Some((new_path, _)) = candidate else {
                // No `.md` to adopt this orphan to → report it (a deleted doc, or a
                // strand whose `.md` the per-file loop will quarantine). Label with the
                // recovered old path when available, else the UUID.
                let label = old_path.unwrap_or_else(|| uuid.to_string());
                warn!("Orphaned content doc (no matching .md): {}", label);
                report.orphaned.push(label);
                continue;
            };
            let new_path = new_path.clone();

            // Register a node at the new path under the orphan's UUID. The content
            // `.loro` is already correctly named `docs/<uuid>.loro` — nothing is
            // relocated or rewritten, which is what keeps the move zero-content.
            let fingerprint = content_version_fingerprint(&orphan_doc.version());
            self.index()
                .register_document(&new_path, &uuid, &fingerprint)?;
            self.documents_mut().insert(new_path.clone(), orphan_doc);

            match old_path {
                Some(old) => {
                    info!("Native-move adopt: {} -> {} ({})", old, new_path, uuid);
                    report.moved.push(FileMove {
                        from: old,
                        to: new_path.clone(),
                    });
                }
                None => {
                    info!(
                        "Adopting orphaned content doc (no node): {} ({})",
                        new_path, uuid
                    );
                    report.adopted.push(new_path.clone());
                }
            }
            matched.insert(new_path);
        }

        Ok(matched)
    }

    /// Reconcile a single `.md` file against its Index/content state.
    ///
    /// Classifies the file by the three axes the decision turns on, then `match`es
    /// over the tuple so every case is enumerable and the dangerous ones fall out of
    /// exhaustiveness rather than a hand-written guard — this IS INV-7's mechanism.
    ///
    /// Axes:
    /// - `has_loro`:   the path's live node has its `docs/<uuid>.loro` on disk.
    /// - `has_node`:   an *alive* Index node exists for this path.
    /// - `tombstoned`: the Index marks this path deleted (alive-wins: a path with an
    ///   alive node is never tombstoned after `rebuild_caches`).
    ///
    /// Under UUID keying `has_loro ⟹ has_node` (a content `.loro` is located via its
    /// node's UUID), so the `(loro, no node)` adopt arm that `sync-core`'s per-file
    /// loop carried does not apply here — the orphan-adopt is content-paired in
    /// `adopt_orphans` (OQ-3 removed the per-path stored path it would have keyed on).
    async fn reconcile_one_file(&self, path: &str, report: &mut ReconcileReport) -> Result<()> {
        let has_node = self.index().node_for_path(path).is_some();
        let has_loro = match self.uuid_for_path(path) {
            Some(uuid) => self.fs().exists(&content_doc_path(&uuid)).await?,
            None => false,
        };
        let tombstoned = self.index().is_path_deleted(path);

        match (has_loro, has_node, tombstoned) {
            // Tombstoned (with or without a `.loro`) → quarantine; NEVER resurrect.
            // A `.md` whose Index state is tombstoned is an untracked strand (a
            // historical-bug or offline-window leftover), not a new file: move it to
            // `.trash/` rather than re-minting a node. quarantine_orphan's own A1 guard
            // short-circuits if an alive node occupies the path, so the stale-armed
            // window is safe. Inner log-and-continue: one orphan failing to quarantine
            // must not block the load, and its specific message is more useful than the
            // caller's generic one.
            (_, _, true) => {
                if let Err(e) = self.quarantine_orphan(path).await {
                    warn!("Failed to quarantine orphan {}: {}", path, e);
                } else {
                    report.quarantined.push(path.to_string());
                }
            }

            // Live node + its `.loro` present → reconcile the content: if the `.md`
            // changed externally, diff-merge it into the doc. on_file_changed is the
            // proven Flow-1 path (it loads the doc from `docs/<uuid>.loro`, diff-merges,
            // rewrites only on a real change, and bumps `content_version`), so the
            // reindex is a delegation rather than a re-implementation of needs_reindex.
            (true, true, false) => {
                if self.on_file_changed(path).await? {
                    info!("File modified externally, re-indexing: {}", path);
                    report.reindexed.push(path.to_string());
                }
            }

            // Live node but its `.loro` is gone (a content file deleted out from under a
            // node — a crash/corruption edge). Non-destructive: leave it for the next
            // sync to backfill from the peer that still holds the document. Recreating
            // it from the `.md` would mint a fresh UUID into the content doc that no
            // longer matches the node's UUID, so reconcile does not fabricate it.
            (false, true, false) => {
                warn!(
                    "Live node at {} has no content doc on disk; recovering on next sync",
                    path
                );
            }

            // No live node, not tombstoned → a brand-new file. on_file_changed creates
            // its content doc and registers the node (Flow-1). `has_loro` cannot be true
            // here (it requires a node), so this single arm covers both `(_, false,
            // false)` tuples.
            (_, false, false) => {
                info!("New file detected, indexing: {}", path);
                self.on_file_changed(path).await?;
                report.indexed.push(path.to_string());
            }
        }

        Ok(())
    }

    /// Record alive Index file nodes that have no backing `.md` on disk
    /// (`report.missing_files`) — the inverse of the adopt direction.
    ///
    /// REPORT-ONLY: reconcile does NOT recreate the file from its content doc
    /// (resurrection) and does NOT tombstone the node (deletion-propagation) — both
    /// are data-loss classes. `path_to_node` holds exactly the alive file-node paths,
    /// so it is the cheap source of truth here; the already-scanned `md_files` set is
    /// consulted first to skip an fs probe for the common (present) case.
    async fn report_missing_files(&self, md_files: &HashSet<String>, report: &mut ReconcileReport) {
        // Collect the alive paths up front (releasing the cache borrow before the fs
        // probes — never hold a lock guard across an await).
        let alive_paths: Vec<String> = self.index().path_to_node().keys().cloned().collect();

        for path in alive_paths {
            if md_files.contains(&path) {
                continue;
            }
            if !self.fs().exists(&path).await.unwrap_or(false) {
                info!("Index node with no backing file (report-only): {}", path);
                report.missing_files.push(path);
            }
        }
    }

    /// Repair stale denormalized `content_version` fingerprints (S3).
    ///
    /// The fingerprint is a derived cache of each content doc's `state_vv()`; a crash
    /// between a content commit and the meta update can leave it stale. Reconcile
    /// recomputes it from the content doc on disk and persists any correction, so the
    /// compare digest (P3) reads a fingerprint that actually matches the content. This
    /// runs after the per-file reindex pass, so a doc whose `.md` changed externally is
    /// already current. Returns whether any fingerprint was repaired (the caller folds
    /// this into the decision to persist the Index).
    async fn repair_content_versions(&self) -> Result<bool> {
        // Snapshot the alive (path, node) pairs before the awaits.
        let alive: Vec<(String, loro::TreeID)> = self
            .index()
            .path_to_node()
            .iter()
            .map(|(p, id)| (p.clone(), *id))
            .collect();

        let mut repaired = false;
        for (path, node) in alive {
            let Some(uuid) = self.index().node_uuid(&node) else {
                continue;
            };
            let loro_path = content_doc_path(&uuid);
            // No content doc on disk → nothing to recompute against (the node-with-no-
            // `.loro` edge handled above); skip rather than fabricate.
            let Ok(bytes) = self.fs().read(&loro_path).await else {
                continue;
            };
            let Ok(doc) = ContentDoc::from_bytes(&bytes, self.loro_author()) else {
                continue;
            };

            let actual = content_version_fingerprint(&doc.version());
            if self.index().node_content_version(&node) != Some(actual) {
                self.index().set_content_version(&node, &actual)?;
                debug!("Repaired stale content_version for {}", path);
                repaired = true;
            }
        }

        Ok(repaired)
    }

    /// Move a tombstoned disk orphan to `.trash/<path>`.
    ///
    /// A disk orphan is a `.md` on disk whose Index state is tombstoned
    /// (`is_path_deleted`). Quarantining is reversible (the file is preserved under
    /// `.trash/`, which `list_files` excludes) and touches only disk — never the Index
    /// tree — so it cannot fight the resurrection guard or a delete, which read the
    /// same deleted-paths set.
    ///
    /// A1 guard: if a live node currently occupies the path, the file is NOT an orphan
    /// no matter how the deleted-paths set looks (a delete inserts synchronously and a
    /// local re-create does not clear it, so a path can be stale-tombstoned while
    /// carrying a freshly re-created live node). The alive-node check closes that
    /// data-loss path in every calling context.
    pub(crate) async fn quarantine_orphan(&self, path: &str) -> Result<()> {
        // A1: an alive node at the path means this is not an orphan — never quarantine.
        if self.index().node_for_path(path).is_some() {
            return Ok(());
        }

        let bytes = self.fs().read(path).await?;

        // Pick the trash destination. Crash-idempotency: if a prior quarantine wrote
        // `.trash/<path>` but failed to delete the original, the orphan sits at BOTH
        // paths. On the next pass we must reuse that identical trash copy and just
        // retry the delete — NOT allocate a new collision suffix, which would let
        // `.trash/<path>.N` grow without bound under a persistent delete failure. A new
        // suffix is only for a genuinely distinct orphan that shares a name.
        let base_dest = format!("{}/{}", TRASH_DIR, path);
        let dest = match self.fs().read(&base_dest).await {
            // Already-trashed identical content → reuse it, skip the write, retry delete.
            Ok(existing) if existing == bytes => base_dest,
            // Occupied by different content → suffix to avoid clobbering a distinct file.
            Ok(_) => {
                let mut n = 1;
                loop {
                    let candidate = format!("{}/{}.{}", TRASH_DIR, path, n);
                    match self.fs().read(&candidate).await {
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
        // crash-idempotency reuse case above skips it). atomic_write (write-temp +
        // rename) keeps the trash copy from being torn if the process crashes mid-write,
        // then delete the original. `write` creates parent dirs (FileSystem contract),
        // so `.trash/` is created on demand.
        if !self.fs().exists(&dest).await? {
            self.fs().atomic_write(&dest, &bytes).await?;
        }

        // The write→delete sequence is non-atomic: if delete fails here, the copy is
        // safely in trash but the original remains. Surface that distinct partial state
        // so the next pass's idempotent reuse (above) is the recovery, not data loss.
        if let Err(e) = self.fs().delete(path).await {
            warn!(
                "Quarantine partially succeeded for {}: copy is in {} but the original \
                 could not be removed ({}); it will be retried on the next reconcile",
                path, dest, e
            );
            return Err(e.into());
        }

        info!("Quarantined disk orphan {} -> {}", path, dest);
        Ok(())
    }

    /// List every document UUID with a content `.loro` in `.sync/docs/`.
    ///
    /// The filename IS the UUID (`<uuid>.loro`), so the parse is the identity read.
    /// A filename that does not parse as a UUID is skipped (foreign debris).
    async fn list_content_docs(&self) -> Result<HashSet<Uuid>> {
        let mut uuids = HashSet::new();

        if !self.fs().exists(DOCS_DIR).await? {
            return Ok(uuids);
        }

        for entry in self.fs().list(DOCS_DIR).await? {
            if entry.is_dir || !entry.name.ends_with(".loro") {
                continue;
            }
            let stem = entry.name.trim_end_matches(".loro");
            if let Ok(uuid) = Uuid::parse_str(stem) {
                uuids.insert(uuid);
            }
        }

        Ok(uuids)
    }
}
