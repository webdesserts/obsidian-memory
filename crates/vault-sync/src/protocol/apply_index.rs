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
//! - **`.loro` cleanup splits by reason, and a move re-materializes the `.md`.** A
//!   content `.loro` is addressed by UUID (`docs/<uuid>.loro`), path-independently.
//!   So a DELETED path's `.loro` is removed (the document is gone), but a MOVED-away
//!   path keeps its `.loro` (the same document lives on at the new path under the
//!   same UUID). Crucially, because a move re-transfers ZERO content (INV-1), the
//!   moved document's `.md` never arrives over the wire — so the receiver must
//!   re-materialize the `.md` at the NEW path from its existing local `<uuid>.loro`.
//!   (The old path-keyed sync sidestepped this by re-sending the whole document under
//!   the new path; the UUID model makes the move zero-content and re-materializes
//!   locally instead.)

use crate::conflict::{NodeKind, StructuralOp, StructuralView, resolve_structure};
use crate::content_doc::{ContentDoc, warn_if_pending};
use crate::fs::FileSystem;
use crate::hash::content_summary;
use crate::index::StructuralNode;
use crate::vault::Vault;

use loro::TreeID;
use std::collections::BTreeMap;
use std::collections::HashMap;
use tracing::{debug, error, warn};

use super::{DocId, Result};

/// A move detected during an Index import: a still-alive node that now lives at a
/// different path than before.
struct DetectedMove {
    /// The path the node moved away from. Its old `.md` is removed only if
    /// [`Self::old_path_vacated`] — when some other node now occupies it (a swap, or a
    /// conflict-cascade survivor taking the loser's old path), the `.md` is the new
    /// occupant's and must be kept.
    from: String,
    /// The path the node now lives at. Its `.md` is ALWAYS re-materialized here from the
    /// path-independent `<uuid>.loro` (the move carried zero content, so the moved `.md`
    /// arrives no other way — INV-1).
    to: String,
    /// The moved document's UUID (stable across the move) — addresses the `.loro`.
    uuid: uuid::Uuid,
    /// Whether no alive node occupies the old path now (so its `.md` is genuinely
    /// stale). `false` for a swap / cascade-survivor that re-took the old path.
    old_path_vacated: bool,
}

/// What an Index import vacated on this device, classified by reason so the caller
/// can both filter document updates and clean/re-materialize the filesystem
/// correctly.
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
    /// Moves: a still-alive node relocated. The old `.md` is removed and the new `.md`
    /// is re-materialized from the unchanged `<uuid>.loro`; the document's update is
    /// NOT filtered (it would apply cleanly at the new path).
    moves: Vec<DetectedMove>,
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
    ///   before this import (a `tree.mov` on the sender, including a conflict-cascade
    ///   rename). The moved `.md` is ALWAYS re-materialized at the new path (zero-content
    ///   move — INV-1), and the old `.md` is removed only if no alive node re-took the
    ///   old path. When some node DID re-take it — a swap (A leaves P1 while B moves into
    ///   P1), or a cascade survivor keeping the loser's old path — the old `.md` is the
    ///   new occupant's and is kept (deleting it would be data loss). This is the B1
    ///   guard, applied per-path in `cleanup_vacated_paths` rather than by suppressing
    ///   the whole move (suppressing it would strand a zero-content move's new-path
    ///   `.md`, which has no document update to backfill it).
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

        // NOTE: the structural-conflict cascade (INV-5.0/5.3) does NOT fire here.
        // It runs in `resolve_structural_conflicts`, called by `apply_response_updates`
        // AFTER `apply_doc_updates` — because a colliding peer's CONTENT lands in the
        // same message's document updates, which apply AFTER this Index-apply (INV-8
        // registry-before-documents). Firing the cascade here would see the colliding
        // node WITHOUT its content (forcing the DP-6 defensive omit), and — proven by
        // experiment — the collision would never re-resolve (once the Index version
        // vectors converge no further Index delta arrives to re-trigger this path). So
        // the cascade fires once the merged Index AND the just-arrived content are both
        // present. The move/delete detection below is unaffected: it consumes the
        // pre-import snapshots captured above, all of which predate the cascade.

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
        // nodes); a moved node stays alive, just under a new path. Capture the new
        // path and the (stable) UUID so the `.md` can be re-materialized there.
        //
        // A move's `.md` re-materialization at the NEW path always runs — that is the
        // defining behavior of a zero-content move (INV-1): the moved `.md` arrives no
        // other way. This matters for a conflict-cascade rename, which IS a zero-content
        // `tree.mov`: when one replica resolves a collision and its rename op
        // propagates here, the moved (renamed) node's conflict `.md` must materialize on
        // this replica, and there is no document update to backfill it. The OLD-path
        // cleanup, by contrast, is conditional (handled in `cleanup_vacated_paths`):
        // when the old path is re-occupied — the swap case, or a cascade survivor taking
        // the loser's old path — the old `.md` is the new occupant's and must NOT be
        // removed. So we detect the move unconditionally and let the cleanup decide
        // whether the old `.md` is stale. (`captured_at` records whether the old path is
        // still occupied so the cleanup needn't re-check the cache.)
        let mut moves: Vec<DetectedMove> = Vec::new();
        {
            let rebuilt = self.index().path_to_node();
            let node_to_new_path: HashMap<TreeID, &String> =
                rebuilt.iter().map(|(p, id)| (*id, p)).collect();

            for (old_path, node_id) in &pre_import_paths {
                if let Some(new_path) = node_to_new_path.get(node_id)
                    && *new_path != old_path
                    && let Some(uuid) = pre_import_uuids.get(old_path)
                {
                    moves.push(DetectedMove {
                        from: old_path.clone(),
                        to: (*new_path).clone(),
                        uuid: *uuid,
                        // The old path is "vacated" (its `.md` is stale and removable)
                        // only if no alive node occupies it now — the swap / cascade-
                        // survivor case keeps it.
                        old_path_vacated: !rebuilt.contains_key(old_path),
                    });
                }
            }
        }

        let vacated = VacatedPaths { deleted, moves };

        // Clean up the filesystem for vacated paths: deletes remove `.md` + `.loro`;
        // a move removes the old `.md` and re-materializes the new `.md` from the
        // unchanged `<uuid>.loro` (the move carried zero content — INV-1).
        self.cleanup_vacated_paths(&vacated).await?;

        // Persist the merged Index, then mark it synced so it reconciles before the
        // next sync import.
        self.index().save_index(self.fs()).await?;
        self.index().sync_state.mark_index_synced();

        debug!(
            "apply_index_updates: complete, deleted={:?}, moves={}",
            vacated.deleted,
            vacated.moves.len()
        );
        Ok(vacated)
    }

    /// Fire the structural-conflict cascade ONCE over the fully-merged state
    /// (INV-5.0/5.3), resolving every distinct-UUID same-path collision into a
    /// deterministic, replica-identical plan and applying it.
    ///
    /// ## Why this fires AFTER document updates, not at the Index-apply
    ///
    /// A colliding peer's NODE arrives in the Index delta but its CONTENT arrives in
    /// the SAME message's document updates, which apply AFTER `apply_index_updates`
    /// (INV-8 registry-before-documents). The cascade needs both contents present to
    /// decide identical-vs-empty-vs-distinct, so it fires from `apply_response_updates`
    /// once both the merged Index and the just-arrived content are on disk. Firing it
    /// inside the Index-apply would force the DP-6 defensive content-omit, and the
    /// collision would then never re-resolve (once the Index version vectors converge,
    /// no further Index delta arrives to re-trigger the Index-apply path).
    ///
    /// ## Cheap when there is no collision
    ///
    /// A collision is a path with ≥2 nodes. A cheap tree scan (no content load) finds
    /// them; with none, this returns immediately — the common no-conflict sync pays
    /// only one tree walk, never a content load. Only when a collision exists is the
    /// full content-bearing view built and the resolver run.
    ///
    /// ## Persistence + DP-5
    ///
    /// `apply_index_updates` already persisted and marked the Index synced before
    /// returning, so any structural ops this produces are re-persisted and re-marked
    /// here. The cascade's own fs cleanup (collapsed/renamed losers) is intentionally
    /// SEPARATE from `cleanup_vacated_paths` (which keys on the import's deletes/moves)
    /// — see [`Self::apply_structural_ops`].
    pub(super) async fn resolve_structural_conflicts(&self) -> Result<()> {
        // Cheap collision gate: group alive nodes by path; a path with ≥2 nodes is a
        // collision. No content is loaded for this check.
        let nodes = self.index().scan_structural_nodes();
        let mut counts: HashMap<&str, usize> = HashMap::new();
        for node in &nodes {
            *counts.entry(node.path()).or_insert(0) += 1;
        }
        if !counts.values().any(|&c| c >= 2) {
            return Ok(());
        }

        // A collision exists — build the FULL content-bearing view (the resolver's
        // fixpoint can rename a loser onto a pre-existing single-occupant path, so the
        // view must carry every occupied path, not just the ≥2-node ones).
        let view = self.build_structural_view(nodes).await;
        let ops = resolve_structure(view);
        if ops.is_empty() {
            return Ok(());
        }

        // `apply_structural_ops` already rebuilt the caches after its tree ops; persist
        // the structural ops and re-mark the Index synced (the apply tail in
        // `apply_index_updates` ran before this).
        self.apply_structural_ops(ops).await?;
        self.index().save_index(self.fs()).await?;
        self.index().sync_state.mark_index_synced();
        Ok(())
    }

    /// Build the [`StructuralView`] snapshot the conflict cascade resolves from the
    /// pre-scanned merged-tree `nodes` (NOT the rebuilt caches).
    ///
    /// Why the tree, not the caches: after `rebuild_caches`, a collision's *loser* is
    /// alive in the tree but has no `path_to_node` slot (the cache keys uniquely per
    /// path), and folders are never cached at all. The scan
    /// ([`Index::scan_structural_nodes`]) surfaces EVERY node at a contested path.
    ///
    /// Each file node is enriched with its [`content_summary`] (an async content-doc
    /// load), so the cascade can decide identical-vs-empty-vs-distinct.
    ///
    /// **Defensive content-load (DP-6 / NFR-6):** if a colliding file's
    /// `docs/<uuid>.loro` is not yet on disk — its content is Flow-2-gated and hasn't
    /// landed — the member is `warn!`'d and OMITTED from the view rather than
    /// panicking. The collision re-resolves on the next exchange once the content
    /// arrives: this can never lose a body, because the next sync re-fires the cascade
    /// with the content present.
    async fn build_structural_view(&self, nodes: Vec<StructuralNode>) -> StructuralView {
        let mut occupants: BTreeMap<String, Vec<NodeKind>> = BTreeMap::new();

        for node in nodes {
            match node {
                StructuralNode::Folder { path, tree_id } => {
                    occupants
                        .entry(path)
                        .or_default()
                        .push(NodeKind::Folder { tree_id });
                }
                StructuralNode::File { path, uuid } => {
                    let loro_path = Self::doc_content_path(&DocId(uuid));
                    // Content not yet on disk → omit the member (DP-6). It is NOT a
                    // collision we can resolve correctly without the body, and the
                    // next exchange re-fires the cascade once the content lands.
                    if !self.fs().exists(&loro_path).await.unwrap_or(false) {
                        warn!(
                            "build_structural_view: no local .loro for {} at {} — omitting from \
                             the cascade view; it re-resolves once the content lands",
                            uuid, path
                        );
                        continue;
                    }
                    let bytes = match self.fs().read(&loro_path).await {
                        Ok(bytes) => bytes,
                        Err(e) => {
                            warn!(
                                "build_structural_view: failed to read .loro for {} at {}: {} — \
                                 omitting from the cascade view",
                                uuid, path, e
                            );
                            continue;
                        }
                    };
                    let doc = match ContentDoc::from_bytes(&bytes, self.loro_author()) {
                        Ok(doc) => doc,
                        Err(e) => {
                            warn!(
                                "build_structural_view: corrupt .loro for {} at {}: {} — \
                                 omitting from the cascade view",
                                uuid, path, e
                            );
                            continue;
                        }
                    };
                    occupants.entry(path).or_default().push(NodeKind::File {
                        uuid,
                        summary: content_summary(&doc),
                    });
                }
            }
        }

        StructuralView { occupants }
    }

    /// Replay a resolved cascade plan against the loro tree + filesystem.
    ///
    /// Two phases keep the materialization correct when several documents share a path
    /// (which is exactly when the cascade runs):
    ///
    /// 1. **Tree ops.** Tombstone each collapsed loser by its `TreeID` (NOT by path —
    ///    the survivor shares the path, so a path-keyed delete could remove the wrong
    ///    node) and remove its content `.loro`; move each renamed loser to its conflict
    ///    path by `TreeID`. Collapses run before renames so a rename onto a
    ///    just-collapsed loser's old path can't hit `MoveTargetExists`.
    /// 2. **Re-materialize.** After rebuilding the caches, re-render the `.md` for every
    ///    path the cascade touched (each survivor path + each conflict path) from the
    ///    `.loro` of the node the merged tree now says owns it. This is the load-bearing
    ///    step: on disk, a colliding path's `.md` was last written by whichever doc
    ///    update landed last (both colliding docs resolve to the same path during
    ///    `apply_doc_updates`), so it may hold the loser's body; re-materializing from
    ///    the resolved owner guarantees the survivor's body sits at the path and each
    ///    loser's body sits at its conflict path. No path is ever fully vacated by the
    ///    cascade (a survivor always backfills the old path), so nothing is removed here
    ///    beyond the collapsed losers' `.loro` files.
    ///
    /// ## Distinct from the existing vacated-path cleanup (DP-5)
    ///
    /// This cleans the cascade's OWN losers — it is intentionally SEPARATE from
    /// `cleanup_vacated_paths`, which keys on the IMPORT's deletes/moves
    /// (`deleted_paths`/`pre_import_paths`, captured before the cascade fired, and
    /// already consumed by the time the cascade runs after `apply_doc_updates`). A
    /// cascade `CollapseFile` is not in `deleted_paths`, and a cascade `RenameFile` of
    /// a pre-existing loser is not detected as a "move" (the survivor occupies the
    /// loser's old path, so the move-detection's `!rebuilt.contains_key(old_path)` guard
    /// skips it). Keeping the two cleanups distinct is what preserves that interplay.
    ///
    /// **S1 (fail loud):** a `MoveTargetExists` on a rename means the resolver's
    /// fixpoint produced two ops targeting one path — a real P2a measure bug. We
    /// `error!` + return `Err` rather than `warn!`+skip, because a silently-skipped
    /// rename drops the loser's conflict file = INV-3 content loss.
    ///
    /// P2a emits only `CollapseFile`/`RenameFile`; the folder-op arms (`MergeFolder`,
    /// `RelocateFile`, `ReviveFolderChain`) land in P2d/P2e.
    async fn apply_structural_ops(&self, ops: Vec<StructuralOp>) -> Result<()> {
        // Partition into collapses (apply first) and renames. The resolver's fixpoint
        // can emit SEVERAL `RenameFile`s for one loser (it relocates the loser step by
        // step in its working view when a conflict path is itself occupied — the
        // transitive case), so collapse them to each loser's FINAL target (the last
        // emitted `to` wins), keeping emission order for the dependency ordering below.
        let mut collapses: Vec<uuid::Uuid> = Vec::new();
        let mut final_target: std::collections::HashMap<uuid::Uuid, String> =
            std::collections::HashMap::new();
        let mut rename_order: Vec<uuid::Uuid> = Vec::new();
        for op in ops {
            match op {
                StructuralOp::CollapseFile { loser } => collapses.push(loser),
                StructuralOp::RenameFile { loser, to } => {
                    if final_target.insert(loser, to).is_none() {
                        rename_order.push(loser);
                    }
                }
                StructuralOp::MergeFolder { .. }
                | StructuralOp::RelocateFile { .. }
                | StructuralOp::ReviveFolderChain { .. } => {
                    // The folder cases are added in P2d/P2e; the P2a resolver never
                    // emits them, so reaching one here is a forward-compat gap, not a
                    // runtime path. Skip rather than fail (the resolver is the gate).
                    debug!("apply_structural_ops: folder op not yet supported, skipping: {op:?}");
                }
            }
        }

        // Phase 1 — tree ops. Collect every path the cascade touches so phase 2 can
        // re-materialize each from its resolved owner.
        let mut touched_paths: std::collections::BTreeSet<String> =
            std::collections::BTreeSet::new();

        for loser in collapses {
            if let Some(path) = self.collapse_loser_node(loser).await? {
                touched_paths.insert(path);
            }
        }

        // Apply renames in DEPENDENCY order: a loser can only move into its final path
        // once that path is free, but one loser's final path may be ANOTHER loser's
        // current path (the transitive case — e.g. loser U2 wants the conflict path that
        // occupant U3 currently sits at, while U3 moves further out). So apply any
        // rename whose target is currently unoccupied, and loop until all are placed.
        // Targets embed the loser's own full UUID (INV-5.2), so they are unique and form
        // no cycle — a pass that places nothing while renames remain is a resolver bug
        // (S1), surfaced loudly. The bound is the rename count (each iteration of the
        // outer loop places ≥1).
        let mut pending: Vec<uuid::Uuid> = rename_order;
        while !pending.is_empty() {
            let mut placed_any = false;
            let mut still_pending = Vec::new();
            for loser in pending {
                let to = final_target[&loser].clone();
                // The target is free iff no node currently occupies it.
                if self.index().node_for_path(&to).is_none() {
                    if let Some(from) = self.rename_loser_node(loser, &to)? {
                        touched_paths.insert(from);
                        touched_paths.insert(to);
                    }
                    placed_any = true;
                } else {
                    still_pending.push(loser);
                }
            }
            if !placed_any {
                // No rename could be placed yet every remaining target is occupied — a
                // cycle the UUID-unique naming should make impossible. Fail loud (S1):
                // silently skipping would drop conflict files (INV-3 loss).
                let blocked: Vec<String> = still_pending
                    .iter()
                    .map(|u| format!("{u} -> {}", final_target[u]))
                    .collect();
                error!(
                    "apply_structural_ops: deadlocked placing conflict renames (every \
                     remaining target occupied) — a resolver bug: {blocked:?}"
                );
                return Err(crate::index::IndexError::MoveTargetExists(format!(
                    "cascade rename deadlock: {blocked:?}"
                ))
                .into());
            }
            pending = still_pending;
        }

        // Re-derive the caches so the resolved tree's path→owner mapping is current,
        // then re-materialize every touched path from the node that now owns it.
        self.index().rebuild_caches();
        for path in touched_paths {
            self.rematerialize_owner_md(&path).await?;
        }

        Ok(())
    }

    /// Tombstone a collapsed loser by its `TreeID` and drop its content `.loro`,
    /// returning the path it occupied (which the survivor keeps) so the caller can
    /// re-materialize the survivor's `.md` there.
    ///
    /// The `.md` at the shared path is NOT removed and the path's cached doc is left
    /// in place — the survivor keeps both, and the collapsed loser's content is
    /// identical to, or empty against, the survivor (INV-3-safe). Only the loser's
    /// own `docs/<loser-uuid>.loro` is removed.
    async fn collapse_loser_node(&self, loser: uuid::Uuid) -> Result<Option<String>> {
        let Some(node) = self.index().find_node_by_uuid(&loser) else {
            debug!("collapse_loser_node: no node for {loser} — already resolved");
            return Ok(None);
        };
        let Some(path) = self.index().path_for_node(&node) else {
            debug!("collapse_loser_node: no path for {loser} — already resolved");
            return Ok(None);
        };

        // Tombstone THIS node by id (the survivor shares the path; a path-keyed delete
        // could remove the survivor instead).
        self.index().delete_node_by_id(node, &path)?;

        // Remove only the loser's content `.loro`; the survivor keeps the `.md`.
        let loro_path = Self::doc_content_path(&DocId(loser));
        if self.fs().exists(&loro_path).await.unwrap_or(false)
            && let Err(e) = self.fs().delete(&loro_path).await
        {
            warn!("collapse_loser_node: failed to delete .loro {loro_path}: {e}");
        }

        debug!("collapse_loser_node: collapsed {loser} (was at {path})");
        Ok(Some(path))
    }

    /// Move a renamed loser to its conflict path by its `TreeID` (a pure-structural
    /// `tree.mov` — UUID + content preserved), returning the old path so the caller can
    /// re-materialize the survivor there.
    ///
    /// No `.md` is written or removed here — phase 2 re-materializes both the old path
    /// (now the survivor's) and the conflict path (now the loser's) from their resolved
    /// owners. A `MoveTargetExists` fails loudly (S1).
    fn rename_loser_node(&self, loser: uuid::Uuid, to: &str) -> Result<Option<String>> {
        let Some(node) = self.index().find_node_by_uuid(&loser) else {
            debug!("rename_loser_node: no node for {loser} — already resolved");
            return Ok(None);
        };
        let Some(from) = self.index().path_for_node(&node) else {
            debug!("rename_loser_node: no path for {loser} — already resolved");
            return Ok(None);
        };

        // Move THIS node by id (the survivor shares `from`; a path-keyed move could
        // relocate the survivor instead).
        if let Err(e) = self.index().move_node_by_id(node, &from, to) {
            error!(
                "rename_loser_node: move {from} -> {to} failed for loser {loser}: {e} — \
                 the cascade plan is inconsistent (a resolver measure bug); failing loudly \
                 rather than dropping the conflict file (INV-3)"
            );
            return Err(e.into());
        }

        debug!("rename_loser_node: renamed {loser} ({from} -> {to})");
        Ok(Some(from.clone()))
    }

    /// Re-materialize the `.md` at `path` from the `.loro` of the node the (rebuilt)
    /// merged tree says owns it — the cascade's correctness step.
    ///
    /// A colliding path's on-disk `.md` may hold the wrong document's body after
    /// `apply_doc_updates` (both colliding docs resolved to the same path, so the last
    /// write won); this rewrites it from the resolved owner. Marks the path synced
    /// first (echo detection) and refreshes the in-memory document cache. If no node
    /// owns `path` (defensive — the cascade should always leave a survivor), the stale
    /// `.md` is removed instead of left dangling.
    async fn rematerialize_owner_md(&self, path: &str) -> Result<()> {
        let Some(uuid) = self.uuid_for_path(path) else {
            // No owner: the path was fully vacated (not expected for the file cascade,
            // which always leaves a survivor). Remove the stale `.md` rather than leave
            // an untracked orphan.
            debug!("rematerialize_owner_md: no owner for {path} — removing stale .md");
            self.remove_md_file(path).await;
            self.documents_mut().remove(path);
            return Ok(());
        };

        let loro_path = Self::doc_content_path(&DocId(uuid));
        if !self.fs().exists(&loro_path).await.unwrap_or(false) {
            // The owner's content never landed on this replica — boot reconcile / a
            // later full sync backstops the `.md`. (Should not happen post-cascade: the
            // owner's content was present in the view that selected it.)
            warn!(
                "rematerialize_owner_md: no local .loro for owner {uuid} of {path} — \
                 .md absent until reconcile backstops it"
            );
            return Ok(());
        }

        let bytes = self.fs().read(&loro_path).await?;
        let doc = ContentDoc::from_bytes(&bytes, self.loro_author())?;
        self.mark_synced(path);
        self.fs().write(path, doc.to_markdown().as_bytes()).await?;
        self.documents_mut().insert(path.to_string(), doc);
        debug!("rematerialize_owner_md: rendered owner {uuid} at {path}");
        Ok(())
    }

    /// Filesystem cleanup for vacated paths.
    ///
    /// Deletes remove the `.md` and the `docs/<uuid>.loro` (the document is gone). A
    /// move ALWAYS re-materializes the `.md` at the NEW path from the unchanged
    /// `<uuid>.loro` (zero-content move — the new-path `.md` arrives no other way,
    /// INV-1), and removes the OLD `.md` only if the old path is genuinely vacated
    /// ([`DetectedMove::old_path_vacated`]); a swap or a conflict-cascade survivor that
    /// re-took the old path keeps it. Every fs mutation marks the path synced first
    /// (echo detection).
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

        for mv in &vacated.moves {
            // Remove the stale `.md` at the old path ONLY if no node re-took it. A swap
            // (another node moved in) or a cascade survivor (kept the loser's old path)
            // leaves the old `.md` belonging to the new occupant — removing it would
            // delete a live file.
            if mv.old_path_vacated {
                self.remove_md_file(&mv.from).await;
                self.documents_mut().remove(&mv.from);
            }

            // Re-materialize the `.md` at the new path from the unchanged `<uuid>.loro`.
            // The content never crossed the wire (zero-content move), so the receiver
            // renders it locally from the document it already holds.
            if let Err(e) = self.rematerialize_moved_md(mv).await {
                warn!(
                    "apply_index_updates: failed to re-materialize moved doc {} at {}: {}",
                    mv.uuid, mv.to, e
                );
            }
        }

        Ok(())
    }

    /// Re-materialize a moved document's `.md` at its new path from the local
    /// `<uuid>.loro` (the path-independent content the receiver already holds).
    ///
    /// Marks the new path synced before the write (echo detection), so the local file
    /// watcher recognizes our own materialization and does not re-broadcast it.
    async fn rematerialize_moved_md(&self, mv: &DetectedMove) -> Result<()> {
        let loro_path = Self::doc_content_path(&DocId(mv.uuid));
        // The `.loro` should be present (a move keeps it); if it is genuinely absent
        // (e.g. this device never had the document materialized), there is nothing to
        // render here. The moved `.md` will be absent at the new path on this receiver
        // until boot reconcile / a later full sync backstops it — a user-visible
        // content gap, so warn rather than log quietly.
        if !self.fs().exists(&loro_path).await? {
            warn!(
                "rematerialize_moved_md: no local .loro for {} — moved .md absent at {} until reconcile backstops it",
                mv.uuid, mv.to
            );
            return Ok(());
        }

        let bytes = self.fs().read(&loro_path).await?;
        let doc = ContentDoc::from_bytes(&bytes, self.loro_author())?;
        self.mark_synced(&mv.to);
        self.fs()
            .write(&mv.to, doc.to_markdown().as_bytes())
            .await?;
        self.documents_mut().insert(mv.to.clone(), doc);
        debug!("rematerialize_moved_md: rendered {} at {}", mv.uuid, mv.to);
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
