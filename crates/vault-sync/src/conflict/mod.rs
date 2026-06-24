//! The pure structural-conflict resolver — `resolve_structure`.
//!
//! When two replicas independently put different things at the same vault path,
//! the merged loro tree can hold several nodes at one display path: two distinct
//! documents, two folders, a file and a folder, or a live node hanging under a
//! folder one replica deleted. The library resolves every such collision with
//! ONE deterministic, side-effect-free pass over the merged-state snapshot —
//! [`resolve_structure`] — so that every replica computes the *identical* plan of
//! structural operations regardless of the order it applied the inbound CRDT ops
//! (INV-5.3). This module is that pass. It touches no filesystem, no [`Index`],
//! and mutates no loro document; it consumes a [`StructuralView`] snapshot and
//! returns a [`Vec<StructuralOp>`] the apply side (P2b) replays.
//!
//! [`Index`]: crate::index::Index
//!
//! ## Why one fixpoint instead of three passes
//!
//! The spec defines three structural rules — folder-merge, file-vs-folder, file
//! cascade — that all read the merged tree and all *mutate paths*, so each rule's
//! output is another rule's input (a folder-merge can surface a same-name file
//! collision). Run as separate passes that call each other, the order they fire
//! at interacting paths would affect the relocation/conflict targets, and two
//! replicas applying the inbound ops in different valid orders could land on
//! *different* final states even though the CRDT tree converges. So they are
//! folded into one fixpoint with a **pinned canonical order** and a
//! **strictly-decreasing lexicographic termination measure** (INV-5.3(a)/(b)).
//! This is the same convergence discipline the file cascade already needs
//! internally (fire once on the fully-merged state, whole-group, never
//! iterative-pairwise — INV-5.0/5.1), lifted up one level to operate *between*
//! rule families.
//!
//! Chunk P2a builds the fixpoint skeleton + the termination measure + the
//! **file-cascade** case only; the folder cases (`MergeFolder`, `RelocateFile`)
//! are added into the *same* function in P2d. The full [`StructuralOp`]
//! vocabulary and [`StructuralView`] shape are defined now so those chunks slot
//! in without churning the types or the apply side.
//!
//! Folder-orphan rescue (a concurrent add swept by a peer's folder delete) is
//! deliberately NOT a resolver case: it reads loro's deleted-node enumeration,
//! not the path-keyed alive-node [`StructuralView`] this pass operates on, so it
//! lives in a separate post-merge pass ([`crate::protocol`]'s `rescue_swept_orphans`).

use crate::hash::ContentSummary;
use loro::TreeID;
use std::collections::BTreeMap;
use uuid::Uuid;

/// One node sitting at a vault display path, as seen in the merged tree.
///
/// A file node carries its content UUID (its stable identity — survives moves and
/// renames) plus the [`ContentSummary`] the file cascade needs to decide
/// "identical?" and "empty?". A folder node carries only its loro `TreeID`: folders
/// have no content-UUID in the data model (only documents do), so a folder's
/// survivor key is its tree-node identity (DP-1).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    File { uuid: Uuid, summary: ContentSummary },
    Folder { tree_id: TreeID },
}

/// A pure snapshot of every occupied path in the merged tree and the node(s) at it.
///
/// This is the entire input to [`resolve_structure`] — built by the apply side
/// (P2b) from the freshly-merged tree, never touched by the resolver after
/// construction. The `BTreeMap` is load-bearing: its ordered iteration makes the
/// resolver's site-visitation reproducible across replicas (a `HashMap` would let
/// two replicas visit interacting sites in different orders and diverge).
///
/// One path may hold: a single file (a normal note), ≥2 files (a file collision),
/// ≥2 folders (a folder collision), or a file and a folder (file-vs-folder). P2a
/// acts only on the ≥2-file case; the view already carries the others so P2d/P2e
/// need no view change.
#[derive(Debug, Clone, Default)]
pub struct StructuralView {
    pub occupants: BTreeMap<String, Vec<NodeKind>>,
}

/// One resolving operation in the plan [`resolve_structure`] returns.
///
/// The apply side (P2b/P2d/P2e) replays these against the loro tree + filesystem.
/// The plan is globally consistent and collision-free (the fixpoint guarantees no
/// two ops target the same path), so applying it is order-independent. P2a emits
/// only `CollapseFile`/`RenameFile`; the folder variants are defined now so the
/// apply side and the later resolver cases do not churn the enum.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StructuralOp {
    /// Delete the loser's node — its content is identical to, or empty against, the
    /// survivor that keeps the path, so nothing is lost (INV-3-safe).
    CollapseFile { loser: Uuid },
    /// Keep both documents: move the loser to a conflict path derived from its own
    /// full UUID. The user reconciles by hand.
    RenameFile { loser: Uuid, to: String },
    /// Merge two folder nodes at one path: re-parent the loser's children under the
    /// survivor, then tombstone the emptied loser folder. (P2d.)
    MergeFolder { survivor: TreeID, loser: TreeID },
    /// A file and a folder collide at one path; the folder wins, the file relocates
    /// to `<folder>/<filename>`. (P2d.)
    RelocateFile { uuid: Uuid, to: String },
}

/// Resolve every structural collision in `view` into a deterministic plan of ops.
///
/// A **fixpoint** over a working copy of the view: each iteration finds the
/// highest-precedence unresolved collision site, computes its resolution, emits the
/// `StructuralOp`s, and applies them to the working view so the next iteration sees
/// the post-step path state. This is how the rules *compose* without ever touching
/// the filesystem — a rename that lands on an occupied path simply reappears as a
/// new collision the next iteration resolves (the transitive resolution IS the
/// fixpoint; there is no separate recursion).
///
/// The cases, in pinned precedence (INV-5.3(a)): **folder-merge** (slot 1) →
/// **file-vs-folder** (slot 2) → the **file cascade** (slot 3). The shape rules (1,2)
/// settle the tree to a fixpoint — ≤1 node-type per path — before the cascade resolves
/// the document-level collisions the now-stable shape exposes.
///
/// ## Determinism (INV-5.3(c))
///
/// Every input is a pure function of merged tree + content state — no wall-clock,
/// no mtime, no local event order. Sites are visited in the `BTreeMap`'s ordered
/// iteration, the file survivor is the **global minimum UUID**, the folder survivor
/// (P2d) is the **minimum `TreeID`**, and every emitted conflict/relocation name is
/// content-independent. So every replica that reaches the same merged tree returns
/// the identical `Vec<StructuralOp>`.
///
/// ## Termination (INV-5.3(b))
///
/// The measure is the precedence-ordered **lexicographic tuple**
/// `(folder_collision_sites, file_vs_folder_sites, file_collision_sites)`
/// (see [`Measure`]). Each rule strictly decreases its OWN component while only ever
/// raising a strictly-less-significant one, so the tuple strictly decreases
/// lexicographically every step and the fixpoint provably terminates. A
/// `MAX_STEPS` guard sized `O(N²)` in node count panics on a non-terminating bug —
/// a hit means the measure is wrong, so we fail loudly rather than spin.
pub fn resolve_structure(view: StructuralView) -> Vec<StructuralOp> {
    let mut working = view;
    let mut ops = Vec::new();

    // O(N²) step bound (INV-5.3(b)): valid input terminates in O(N²) strict
    // measure decreases, so any run that blows past this is a measure bug, not a
    // large-but-valid input. We panic (mirroring `pump_to_quiescence`'s round cap)
    // rather than silently truncate a divergent plan.
    let node_count = working.occupants.values().map(Vec::len).sum::<usize>();
    let max_steps = 4 * (node_count + 1) * (node_count + 1) + 16;

    let mut steps = 0;
    while let Some(site) = working.next_unresolved_site() {
        steps += 1;
        assert!(
            steps <= max_steps,
            "resolve_structure exceeded {max_steps} steps over {node_count} nodes — \
             the termination measure is not strictly decreasing (a resolver bug)"
        );

        match site {
            Site::FolderMerge { path } => resolve_folder_merge(&mut working, &path, &mut ops),
            Site::FileVsFolder { path } => {
                resolve_file_vs_folder_site(&mut working, &path, &mut ops)
            }
            Site::FileCollision { path } => resolve_file_collision(&mut working, &path, &mut ops),
        }
    }

    ops
}

/// Build the conflict-file path for a renamed loser (INV-5.2 / B2).
///
/// The suffix embeds the loser's **full 36-char UUID**, which makes collision with
/// a real user filename effectively impossible (a user would have to name a file
/// with an exact random UUID). The path stays in the same parent directory as the
/// original, preserves the `.md` extension, and is content-independent — so every
/// replica derives the identical conflict path. Multi-dot stems keep everything
/// before the trailing `.md` as the stem (e.g. `Note.draft.md` → stem `Note.draft`).
///
/// `projects/Note.md` + `7f3a…` → `projects/Note (conflict 7f3a9c21-…-…).md`.
pub fn conflict_name(path: &str, loser_uuid: &Uuid) -> String {
    let (parent, filename) = match path.rfind('/') {
        Some(slash) => (&path[..slash + 1], &path[slash + 1..]),
        None => ("", path),
    };
    // The extension is the trailing `.md`; everything before it is the stem. A path
    // without `.md` (defensive — the cascade only handles markdown nodes) keeps the
    // whole filename as the stem and gets no extension back.
    let (stem, ext) = match filename.strip_suffix(".md") {
        Some(stem) => (stem, ".md"),
        None => (filename, ""),
    };
    format!("{parent}{stem} (conflict {loser_uuid}){ext}")
}

// ---------------------------------------------------------------------------
// Internal: the fixpoint scaffolding (measure + site scan)
// ---------------------------------------------------------------------------

/// The kind of unresolved collision at a path, in pinned-precedence order.
///
/// The structural-SHAPE sites (`FolderMerge` slot 1, `FileVsFolder` slot 2) outrank
/// the document-level `FileCollision` (slot 3): the tree's shape must be resolved to a
/// fixpoint — ≤1 node-type per path — before file collisions are settled, because a
/// folder-merge or a file-vs-folder relocate can CREATE or DISSOLVE a file collision,
/// so file collisions must be resolved against the FINAL shape, never an intermediate
/// one (the between-families analogue of INV-5.0).
enum Site {
    /// ≥2 folder nodes at one path (slot 1) → merge into the min-TreeID survivor.
    FolderMerge { path: String },
    /// A file node and a folder node at one path (slot 2) → folder wins, file relocates.
    FileVsFolder { path: String },
    /// ≥2 file nodes at one path (slot 3) → the file cascade (INV-5.1).
    FileCollision { path: String },
}

/// The strictly-decreasing lexicographic termination measure (INV-5.3(b)).
///
/// The components are ordered most-significant first, matching the resolver's
/// pinned precedence (1)→(3): a higher-precedence rule may only ever surface a
/// strictly-lower-precedence site, never a higher one, so each step strictly
/// decreases this tuple in dictionary order. A flat *sum* of the components is NOT
/// strictly decreasing (a relocate onto an occupied path is net-zero on a sum) —
/// that was the B1 plan-review finding; the lexicographic tuple is what makes
/// termination provable.
///
/// The production `resolve_structure` loop bounds itself with the cheaper
/// `MAX_STEPS` step count rather than recomputing this measure each iteration — the
/// measure is the termination *proof obligation* (asserted in tests), not the
/// runtime mechanism. So it is constructed only in test code today.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Measure {
    /// # paths with ≥2 alive folder nodes (P2d).
    folder_collision_sites: usize,
    /// # paths holding BOTH a file node and a folder node (P2d).
    file_vs_folder_sites: usize,
    /// # paths with ≥2 alive file nodes (P2a).
    file_collision_sites: usize,
}

impl StructuralView {
    /// Find the highest-precedence unresolved collision site, or `None` when the
    /// view is fully resolved (every path holds exactly one materializable node).
    ///
    /// The scan walks the pinned precedence (INV-5.3(a)): it returns the highest-
    /// precedence site found ANYWHERE before considering any lower-precedence one — so
    /// a folder-merge at any path preempts every file-vs-folder, which preempts every
    /// file collision. The resolver then settles that site and re-scans, which is what
    /// drives the shape rules to a fixpoint before the cascade runs. Within each slot,
    /// sites are visited in the `occupants` `BTreeMap`'s ordered iteration, so the choice
    /// is content-independent and identical on every replica.
    fn next_unresolved_site(&self) -> Option<Site> {
        // Slot 1 — folder collision (≥2 folder nodes at one path). Scanned first so the
        // tree's folder shape settles before file-vs-folder and the file cascade.
        for (path, occupants) in &self.occupants {
            if occupants.iter().filter(|n| n.is_folder()).count() >= 2 {
                return Some(Site::FolderMerge { path: path.clone() });
            }
        }

        // Slot 2 — file-vs-folder (a file node AND a folder node at one path). Reached
        // only once no path has ≥2 folders, so a matching path has exactly one folder.
        for (path, occupants) in &self.occupants {
            let has_file = occupants.iter().any(|n| n.is_file());
            let has_folder = occupants.iter().any(|n| n.is_folder());
            if has_file && has_folder {
                return Some(Site::FileVsFolder { path: path.clone() });
            }
        }

        // Slot 3 — file collision (≥2 file nodes at one path).
        for (path, occupants) in &self.occupants {
            if occupants.iter().filter(|n| n.is_file()).count() >= 2 {
                return Some(Site::FileCollision { path: path.clone() });
            }
        }
        None
    }

    /// Compute the lexicographic termination measure of the current view.
    ///
    /// Used by the tests to prove each step strictly decreases the measure; the
    /// resolver loop itself is bounded by the cheaper `MAX_STEPS` step count.
    #[cfg(test)]
    fn measure(&self) -> Measure {
        let mut m = Measure {
            folder_collision_sites: 0,
            file_vs_folder_sites: 0,
            file_collision_sites: 0,
        };
        for occupants in self.occupants.values() {
            let files = occupants.iter().filter(|n| n.is_file()).count();
            let folders = occupants.iter().filter(|n| n.is_folder()).count();
            if folders >= 2 {
                m.folder_collision_sites += 1;
            }
            if files >= 1 && folders >= 1 {
                m.file_vs_folder_sites += 1;
            }
            if files >= 2 {
                m.file_collision_sites += 1;
            }
        }
        m
    }
}

impl NodeKind {
    fn is_file(&self) -> bool {
        matches!(self, NodeKind::File { .. })
    }

    fn is_folder(&self) -> bool {
        matches!(self, NodeKind::Folder { .. })
    }

    /// The content UUID of a file node; `None` for a folder (folders have no UUID).
    fn file_uuid(&self) -> Option<Uuid> {
        match self {
            NodeKind::File { uuid, .. } => Some(*uuid),
            NodeKind::Folder { .. } => None,
        }
    }
}

// ---------------------------------------------------------------------------
// The file-cascade case (INV-5.1) — whole-group, not iterative-pairwise
// ---------------------------------------------------------------------------

/// Resolve one file-collision site (≥2 distinct-UUID file nodes at `path`) over the
/// **whole group at once** (INV-5.1), emitting the ops and mutating `working` so the
/// next fixpoint iteration sees the result.
///
/// Three steps, in order:
/// 1. **Collapse identicals.** Partition the file members by content hash; within
///    each hash-equal cluster all but the min-UUID member collapse (their content is
///    identical to the survivor — INV-3-safe). Covers empty-vs-empty (blank docs
///    hash equal → collapse to min-UUID, OQ-7).
/// 2. **Empty loses to non-empty.** Among the surviving representatives, if any is
///    non-empty, every *empty* representative collapses (it has no content to lose,
///    INV-3-safe). This precedes the min-UUID tiebreak — an empty doc with the
///    smaller UUID still loses.
/// 3. **Keep-both, min-UUID survivor.** The global minimum UUID among the remaining
///    non-empty representatives wins `path`; every other is renamed to its own
///    `(conflict <full-uuid>)` file.
///
/// Mutating the working view: collapsed losers are removed entirely; the survivor
/// stays at `path`; each renamed loser is *moved* to its conflict path (removed from
/// `path`, re-inserted as a file node at the new path). A rename onto an occupied
/// path therefore re-surfaces as a fresh collision the next iteration resolves — the
/// transitive resolution (INV-5.2) with no separate recursion.
fn resolve_file_collision(working: &mut StructuralView, path: &str, ops: &mut Vec<StructuralOp>) {
    // The file members at this path, paired with their summaries. (Folder members,
    // if any in a later chunk, are left untouched by the file cascade.)
    let members: Vec<(Uuid, ContentSummary)> = working
        .occupants
        .get(path)
        .map(|nodes| {
            nodes
                .iter()
                .filter_map(|n| match n {
                    NodeKind::File { uuid, summary } => Some((*uuid, *summary)),
                    NodeKind::Folder { .. } => None,
                })
                .collect()
        })
        .unwrap_or_default();

    // Step 1 — collapse identicals. Cluster by content hash, keep each cluster's
    // min-UUID as its representative, collapse the rest.
    let mut clusters: BTreeMap<[u8; 32], Vec<(Uuid, ContentSummary)>> = BTreeMap::new();
    for member in members {
        clusters
            .entry(member.1.content_hash)
            .or_default()
            .push(member);
    }
    let mut representatives: Vec<(Uuid, ContentSummary)> = Vec::new();
    for cluster in clusters.into_values() {
        let rep = *cluster
            .iter()
            .min_by_key(|(uuid, _)| *uuid)
            .expect("each content-hash cluster has at least one member");
        for (uuid, _) in &cluster {
            if *uuid != rep.0 {
                ops.push(StructuralOp::CollapseFile { loser: *uuid });
            }
        }
        representatives.push(rep);
    }

    // Step 2 — empty loses to non-empty. If any representative is non-empty, every
    // empty representative collapses (it carries no content). If all are empty, step
    // 1 already collapsed them to a single representative, so this is a no-op.
    let any_non_empty = representatives.iter().any(|(_, s)| !s.is_empty);
    if any_non_empty {
        representatives.retain(|(uuid, s)| {
            if s.is_empty {
                ops.push(StructuralOp::CollapseFile { loser: *uuid });
                false
            } else {
                true
            }
        });
    }

    // Step 3 — keep-both, min-UUID survivor. The global min-UUID representative wins
    // `path`; every other is renamed to its own conflict file.
    let survivor_uuid = representatives
        .iter()
        .map(|(uuid, _)| *uuid)
        .min()
        .expect("a resolved file-collision group always has ≥1 surviving representative");

    let mut renames: Vec<(Uuid, String)> = Vec::new();
    for (uuid, _) in &representatives {
        if *uuid != survivor_uuid {
            let to = conflict_name(path, uuid);
            ops.push(StructuralOp::RenameFile {
                loser: *uuid,
                to: to.clone(),
            });
            renames.push((*uuid, to));
        }
    }

    // Apply the resolution to the working view so the fixpoint composes:
    // the file members at `path` are replaced by the survivor alone, and each renamed
    // loser is re-inserted as a file node at its conflict path (collapsed losers just
    // vanish). Folder members at `path` (later chunks) are preserved.
    apply_file_resolution(working, path, survivor_uuid, &renames);
}

/// Rewrite the working view after a file-collision group at `path` is resolved.
///
/// Drops every file member at `path` except the survivor, then inserts each renamed
/// loser as a fresh `File` node at its conflict path (carrying its original UUID +
/// summary, since a rename is pure-structural — content is unchanged). A conflict
/// path that already holds a live node thus becomes a new collision site the next
/// fixpoint iteration resolves.
fn apply_file_resolution(
    working: &mut StructuralView,
    path: &str,
    survivor_uuid: Uuid,
    renames: &[(Uuid, String)],
) {
    // Pull each renamed loser's full node out of `path` so it can be re-homed with
    // its original summary intact, then collapse `path` down to non-losing nodes.
    let mut moved_nodes: Vec<(String, NodeKind)> = Vec::new();
    if let Some(nodes) = working.occupants.get_mut(path) {
        for (loser_uuid, to) in renames {
            if let Some(node) = nodes
                .iter()
                .find(|n| n.file_uuid() == Some(*loser_uuid))
                .copied()
            {
                moved_nodes.push((to.clone(), node));
            }
        }
        // Keep folder members and the surviving file; drop collapsed + renamed-away
        // file members.
        nodes.retain(|n| match n.file_uuid() {
            Some(uuid) => uuid == survivor_uuid,
            None => true, // a folder member — untouched by the file cascade
        });
        if nodes.is_empty() {
            working.occupants.remove(path);
        }
    }

    for (to, node) in moved_nodes {
        working.occupants.entry(to).or_default().push(node);
    }
}

// ---------------------------------------------------------------------------
// The folder-merge case (INV-1.5c / OQ-5) — min-TreeID survivor, union children
// ---------------------------------------------------------------------------

/// Resolve one folder-collision site (≥2 distinct-TreeID folder nodes at `path`) by
/// merging into the **min-TreeID** survivor (INV-1.5c), emitting one `MergeFolder` per
/// loser and mutating `working` so the next fixpoint iteration sees the result.
///
/// **Survivor key = min-TreeID** (folders have no content-UUID — DP-1; `TreeID` derives
/// `Ord`, so the minimum is a globally-agreed total order, identical on every replica).
/// Whole-group, not iterative-pairwise: EVERY non-min folder loses to the SAME survivor.
///
/// **Why the working view only drops the loser folder node.** The view is keyed by
/// display PATH, and both folders sit at the same path `P`, so their children already
/// occupy the SAME child paths (`P/child`) in the view — the union is already reflected
/// by the path-keying. So reflecting the merge is just removing the loser folder node
/// from `P`; any same-name file collision (≥2 files at `P/Notes.md`) or same-name
/// sub-folder collision (≥2 folders at `P/sub`) is already present in the view and is
/// resolved by a lower-precedence case (file cascade) or by folder-merge again
/// (transitively) on a later iteration. (The APPLY side, by contrast, must actually
/// re-parent the loser's children under the survivor before tombstoning it, because
/// there the children hang under genuinely-distinct tree nodes — see
/// `apply_structural_ops`.)
///
/// **Termination contribution:** each `MergeFolder` removes one folder node at `P`,
/// strictly decreasing the folder-collision-site count (the slot-2 shape component);
/// the collisions its union "surfaces" were already counted in the lower components, so
/// no higher component ever rises (INV-5.3(b)).
fn resolve_folder_merge(working: &mut StructuralView, path: &str, ops: &mut Vec<StructuralOp>) {
    let folders: Vec<TreeID> = working
        .occupants
        .get(path)
        .map(|nodes| {
            nodes
                .iter()
                .filter_map(|n| match n {
                    NodeKind::Folder { tree_id } => Some(*tree_id),
                    NodeKind::File { .. } => None,
                })
                .collect()
        })
        .unwrap_or_default();

    let Some(survivor) = folders.iter().copied().min() else {
        return;
    };

    // Emit a merge for every loser, and drop the loser folder nodes from the working
    // view (the survivor folder stays at `path`). The same-path children are already
    // unioned by the view's path-keying, so nothing else moves here.
    for &loser in &folders {
        if loser != survivor {
            ops.push(StructuralOp::MergeFolder { survivor, loser });
        }
    }
    if let Some(nodes) = working.occupants.get_mut(path) {
        nodes.retain(|n| !matches!(n, NodeKind::Folder { tree_id } if *tree_id != survivor));
        if nodes.is_empty() {
            working.occupants.remove(path);
        }
    }
}

// ---------------------------------------------------------------------------
// The file-vs-folder case (INV-1.5d) — folder wins, file relocates inside
// ---------------------------------------------------------------------------

/// Resolve one file-vs-folder site at `path` (one file node + one folder node) by the
/// DECIDED relocate-inside rule (INV-1.5d): the **folder wins the path**, the file
/// relocates to `<path>/<filename>`, mutating `working` so the next iteration sees it.
///
/// The site is only reached once no path has ≥2 folders (folder-merge has higher
/// precedence), so there is exactly one folder here; if the file's content is needed it
/// is carried on the moved `NodeKind`. The relocation may land on a path already
/// occupied by a live file → a file collision the slot-4 cascade then resolves.
///
/// The pure rule itself — which node wins and where the file goes — is the isolated
/// [`resolve_file_vs_folder`] helper (DP-7, code cleanliness); this wrapper applies its
/// output to the working view.
fn resolve_file_vs_folder_site(
    working: &mut StructuralView,
    path: &str,
    ops: &mut Vec<StructuralOp>,
) {
    let nodes = match working.occupants.get(path) {
        Some(nodes) => nodes,
        None => return,
    };
    let file = nodes.iter().find_map(|n| match n {
        NodeKind::File { uuid, summary } => Some((*uuid, *summary)),
        NodeKind::Folder { .. } => None,
    });
    let folder = nodes.iter().find_map(|n| match n {
        NodeKind::Folder { tree_id } => Some(*tree_id),
        NodeKind::File { .. } => None,
    });
    let (Some((file_uuid, file_summary)), Some(_folder)) = (file, folder) else {
        return;
    };

    let relocate_ops = resolve_file_vs_folder(path, file_uuid);
    ops.extend(relocate_ops.iter().cloned());

    // Apply to the working view: the folder keeps `path`; the file node moves to its
    // relocation target (carrying its summary, since a relocate is pure-structural).
    let to = match relocate_ops.first() {
        Some(StructuralOp::RelocateFile { to, .. }) => to.clone(),
        _ => return,
    };
    if let Some(nodes) = working.occupants.get_mut(path) {
        nodes.retain(|n| n.file_uuid() != Some(file_uuid));
        if nodes.is_empty() {
            working.occupants.remove(path);
        }
    }
    working
        .occupants
        .entry(to)
        .or_default()
        .push(NodeKind::File {
            uuid: file_uuid,
            summary: file_summary,
        });
}

/// The isolated file-vs-folder rule (INV-1.5d / DP-7): the folder wins `folder_path`,
/// the file relocates INSIDE it at `<folder_path>/<filename>` — emitted as a single
/// `RelocateFile`. Kept as one focused helper for code cleanliness.
///
/// `<filename>` is the last path segment of `folder_path` (its basename), so the
/// relocation target is `folder_path` + "/" + that basename — e.g. `Notes.md` →
/// `Notes.md/Notes.md`, `a/b/Notes.md` → `a/b/Notes.md/Notes.md`. The target is
/// content-independent (folder-wins is a fixed rule), so every replica derives the
/// identical relocation. relocate-inside is DECIDED, not swappable (Michael 2026-06-21,
/// reaffirmed 06-22); there is deliberately no "sibling conflict-copy" alternative here.
fn resolve_file_vs_folder(folder_path: &str, file_uuid: Uuid) -> Vec<StructuralOp> {
    let filename = match folder_path.rfind('/') {
        Some(slash) => &folder_path[slash + 1..],
        None => folder_path,
    };
    let to = format!("{folder_path}/{filename}");
    vec![StructuralOp::RelocateFile {
        uuid: file_uuid,
        to,
    }]
}

#[cfg(test)]
mod tests;
