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
//! ## Why one fixpoint instead of four passes
//!
//! The spec defines four structural rules — file cascade, folder-merge,
//! file-vs-folder, folder-revive — that all read the merged tree and all *mutate
//! paths*, so each rule's output is another rule's input (a folder-merge can
//! surface a same-name file collision; a revive can surface a folder-merge). Run
//! as separate passes that call each other, the order they fire at interacting
//! paths would affect the relocation/conflict targets, and two replicas applying
//! the inbound ops in different valid orders could land on *different* final
//! states even though the CRDT tree converges. So they are folded into one
//! fixpoint with a **pinned canonical order** and a **strictly-decreasing
//! lexicographic termination measure** (INV-5.3(a)/(b)). This is the same
//! convergence discipline the file cascade already needs internally (fire once on
//! the fully-merged state, whole-group, never iterative-pairwise — INV-5.0/5.1),
//! lifted up one level to operate *between* rule families.
//!
//! Chunk P2a builds the fixpoint skeleton + the termination measure + the
//! **file-cascade** case only; the folder cases (`MergeFolder`, `RelocateFile`,
//! `ReviveFolderChain`) are added into the *same* function in P2d/P2e. The full
//! [`StructuralOp`] vocabulary and [`StructuralView`] shape are defined now so
//! those chunks slot in without churning the types or the apply side.

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
    /// Revive a deleted folder (and its deleted ancestor chain) to hold a live
    /// descendant a peer concurrently added. (P2e.)
    ReviveFolderChain { path: String },
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
/// P2a implements only the **file-cascade** case (precedence slot 4). The folder
/// cases (slots 1–3) are added into this same loop in P2d/P2e.
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
/// `(revive_sites, folder_collision_sites, file_vs_folder_sites, file_collision_sites)`
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
/// P2a only has `FileCollision` (slot 4). P2d/P2e add the higher-precedence
/// structural-shape variants (revive, folder-merge, file-vs-folder) AHEAD of it.
enum Site {
    FileCollision { path: String },
}

/// The strictly-decreasing lexicographic termination measure (INV-5.3(b)).
///
/// The components are ordered most-significant first, matching the resolver's
/// pinned precedence (1)→(4): a higher-precedence rule may only ever surface a
/// strictly-lower-precedence site, never a higher one, so each step strictly
/// decreases this tuple in dictionary order. A flat *sum* of the components is NOT
/// strictly decreasing (a revive that surfaces a folder collision, or a relocate
/// onto an occupied path, is net-zero on a sum) — that was the B1 plan-review
/// finding; the lexicographic tuple is what makes termination provable. P2a only
/// populates `file_collision_sites`; the full tuple is encoded now so the measure
/// is stable as P2d/P2e add the higher components.
///
/// The production `resolve_structure` loop bounds itself with the cheaper
/// `MAX_STEPS` step count rather than recomputing this measure each iteration — the
/// measure is the termination *proof obligation* (asserted in tests), not the
/// runtime mechanism. So it is constructed only in test code today; P2d/P2e's
/// folder-case resolution will populate the higher components.
#[cfg_attr(not(test), allow(dead_code))]
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct Measure {
    /// # alive nodes whose ancestor chain contains a deleted folder (P2e).
    revive_sites: usize,
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
    /// Sites are scanned in the `occupants` `BTreeMap`'s ordered iteration, so the
    /// choice of which site to resolve next is itself content-independent and
    /// identical on every replica. P2a recognizes only the file-collision site;
    /// P2d/P2e insert the structural-shape sites ahead of it.
    fn next_unresolved_site(&self) -> Option<Site> {
        // Precedence slot 4 — file collision (≥2 file nodes at one path). The
        // higher-precedence structural-shape slots are added in P2d/P2e and must be
        // scanned BEFORE this one when present.
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
            revive_sites: 0,
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

    #[cfg_attr(not(test), allow(dead_code))]
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

#[cfg(test)]
mod tests {
    use super::*;

    // Deterministic UUIDs for the enumerated cascade cases. `Uuid::from_u128`
    // gives a total order that matches the numeric argument, so `U1 < U2 < U3 < U4`
    // makes the min-UUID survivor obvious by construction.
    const U1: u128 = 0x1111_1111_1111_1111_1111_1111_1111_1111;
    const U2: u128 = 0x2222_2222_2222_2222_2222_2222_2222_2222;
    const U3: u128 = 0x3333_3333_3333_3333_3333_3333_3333_3333;
    const U4: u128 = 0x4444_4444_4444_4444_4444_4444_4444_4444;

    fn uuid(n: u128) -> Uuid {
        Uuid::from_u128(n)
    }

    /// A non-empty file node with a content hash derived from `hash_seed` (so two
    /// members share content iff they share a seed) and the given UUID.
    fn file(n: u128, hash_seed: u8) -> NodeKind {
        NodeKind::File {
            uuid: uuid(n),
            summary: ContentSummary {
                content_hash: [hash_seed; 32],
                is_empty: false,
            },
        }
    }

    /// An empty file node (blank body + no frontmatter). All empty docs share the
    /// same materialized markdown, hence the same content hash — the "empty content
    /// hash" — which is what makes two empty docs collapse in step 1 (OQ-7).
    const EMPTY_HASH: u8 = 0xEE;
    fn empty_file(n: u128) -> NodeKind {
        NodeKind::File {
            uuid: uuid(n),
            summary: ContentSummary {
                content_hash: [EMPTY_HASH; 32],
                is_empty: true,
            },
        }
    }

    fn view_at(path: &str, nodes: Vec<NodeKind>) -> StructuralView {
        let mut occupants = BTreeMap::new();
        occupants.insert(path.to_string(), nodes);
        StructuralView { occupants }
    }

    /// Extract `CollapseFile` losers from an op plan, sorted for set comparison.
    fn collapsed(ops: &[StructuralOp]) -> Vec<Uuid> {
        let mut v: Vec<Uuid> = ops
            .iter()
            .filter_map(|op| match op {
                StructuralOp::CollapseFile { loser } => Some(*loser),
                _ => None,
            })
            .collect();
        v.sort();
        v
    }

    /// Extract `(loser, to)` rename pairs from an op plan, sorted for set comparison.
    fn renamed(ops: &[StructuralOp]) -> Vec<(Uuid, String)> {
        let mut v: Vec<(Uuid, String)> = ops
            .iter()
            .filter_map(|op| match op {
                StructuralOp::RenameFile { loser, to } => Some((*loser, to.clone())),
                _ => None,
            })
            .collect();
        v.sort();
        v
    }

    // --- Rule 1: identical content collapses ---

    /// Two files with equal content but distinct UUIDs collapse to the min-UUID
    /// survivor — the larger UUID is deleted, no rename (the content is identical, so
    /// nothing is lost: INV-3-safe).
    #[test]
    fn rule1_identical_content_collapses_to_min_uuid() {
        let ops = resolve_structure(view_at("Note.md", vec![file(U1, 7), file(U2, 7)]));
        assert_eq!(collapsed(&ops), vec![uuid(U2)], "max-UUID loser collapses");
        assert!(
            renamed(&ops).is_empty(),
            "identical content never produces a conflict rename"
        );
    }

    /// Two empty docs (blank body, no frontmatter) hash equal, so they collapse to
    /// the min-UUID survivor under rule 1 — the empty-vs-empty case (OQ-7). No
    /// conflict file is created for two genuinely-empty stubs.
    #[test]
    fn rule1_empty_vs_empty_collapses_to_min_uuid() {
        let ops = resolve_structure(view_at("Note.md", vec![empty_file(U2), empty_file(U1)]));
        assert_eq!(
            collapsed(&ops),
            vec![uuid(U2)],
            "two empty docs collapse to the min-UUID survivor"
        );
        assert!(renamed(&ops).is_empty());
    }

    // --- Rule 2: empty loses to non-empty ---

    /// One empty doc and one non-empty doc at a path: the empty one is dropped (it
    /// has no content to lose, INV-3-safe), the non-empty survives. No conflict file.
    #[test]
    fn rule2_empty_loses_to_non_empty() {
        let ops = resolve_structure(view_at("Note.md", vec![file(U1, 5), empty_file(U2)]));
        assert_eq!(collapsed(&ops), vec![uuid(U2)], "the empty doc is dropped");
        assert!(renamed(&ops).is_empty());
    }

    /// The empty-drop is NOT overridden by the min-UUID tiebreak: even when the empty
    /// doc has the *smaller* UUID, it still loses to a non-empty doc (rule 2 precedes
    /// rule 3). The non-empty doc — despite the larger UUID — survives at the path.
    #[test]
    fn rule2_empty_with_smaller_uuid_still_loses() {
        let ops = resolve_structure(view_at("Note.md", vec![empty_file(U1), file(U2, 5)]));
        assert_eq!(
            collapsed(&ops),
            vec![uuid(U1)],
            "the empty doc loses even though it has the smaller UUID"
        );
        assert!(
            renamed(&ops).is_empty(),
            "the non-empty doc survives at the path — no rename"
        );
    }

    // --- Rule 3: keep both, rename the loser to a full-UUID conflict file ---

    /// Two non-empty distinct docs: the min-UUID survives at the path, the loser is
    /// renamed to a conflict file embedding its OWN full 36-char UUID, in the same
    /// parent directory, `.md` extension preserved. Both documents survive (INV-3).
    #[test]
    fn rule3_keep_both_renames_loser_with_full_uuid() {
        let ops = resolve_structure(view_at("projects/Note.md", vec![file(U1, 1), file(U2, 2)]));
        assert!(
            collapsed(&ops).is_empty(),
            "distinct non-empty docs never collapse"
        );
        let expected_to = format!("projects/Note (conflict {}).md", uuid(U2));
        assert_eq!(
            renamed(&ops),
            vec![(uuid(U2), expected_to.clone())],
            "the max-UUID loser is renamed to a full-UUID conflict file in the same dir"
        );
        // The conflict suffix embeds the FULL 36-char UUID (B2) — collision-proof.
        assert!(
            expected_to.contains(&uuid(U2).to_string()),
            "conflict name embeds the full UUID"
        );
        assert_eq!(uuid(U2).to_string().len(), 36, "UUID is the full 36 chars");
    }

    // --- N≥2 whole-group resolution (B1: global min, not iterative-pairwise) ---

    /// Three+ distinct non-empty files at one path resolve over the WHOLE group:
    /// the global min-UUID wins, every OTHER member gets its own conflict file.
    /// Shuffling the input order must yield the identical op plan (set-equal) — the
    /// resolver is order-independent, the prerequisite for cross-replica determinism.
    #[test]
    fn ngroup_global_min_wins_others_each_get_conflict_file() {
        let nodes = vec![file(U1, 1), file(U2, 2), file(U3, 3), file(U4, 4)];
        let ops = resolve_structure(view_at("Note.md", nodes.clone()));

        // U1 (global min) survives; U2/U3/U4 each get their own conflict file.
        assert!(collapsed(&ops).is_empty());
        let expected = vec![
            (uuid(U2), format!("Note (conflict {}).md", uuid(U2))),
            (uuid(U3), format!("Note (conflict {}).md", uuid(U3))),
            (uuid(U4), format!("Note (conflict {}).md", uuid(U4))),
        ];
        assert_eq!(renamed(&ops), expected);

        // Shuffle the input → identical plan. Every permutation of the same group
        // must converge to the same survivor and the same conflict set (the input
        // arrives in different orders on different replicas).
        for shuffle in permutations(&nodes) {
            let shuffled_ops = resolve_structure(view_at("Note.md", shuffle));
            assert_eq!(
                renamed(&shuffled_ops),
                expected,
                "shuffled input must produce the identical conflict plan"
            );
            assert!(collapsed(&shuffled_ops).is_empty());
        }
    }

    /// The B1 case made concrete: a naive iterative-pairwise resolver (fold "keep the
    /// smaller of the running survivor and the next member") could, depending on
    /// visitation order, pick a different survivor than the global minimum. Here the
    /// whole-group rule must pick the GLOBAL min (U1) regardless of input order — the
    /// pairwise-order-dependent outcome is exactly what would diverge across replicas.
    #[test]
    fn ngroup_picks_global_min_not_pairwise_artifact() {
        // Present the group with the global-min LAST, so a left-fold that committed
        // early to a survivor would be tempted to keep an earlier one.
        let nodes = vec![file(U3, 3), file(U4, 4), file(U2, 2), file(U1, 1)];
        let ops = resolve_structure(view_at("Note.md", nodes));
        // U1 wins (global min); the other three are renamed. If a pairwise artifact
        // had won, U1 would appear as a rename target instead of the survivor.
        let losers: Vec<Uuid> = renamed(&ops).into_iter().map(|(u, _)| u).collect();
        assert_eq!(losers, vec![uuid(U2), uuid(U3), uuid(U4)]);
        assert!(
            !losers.contains(&uuid(U1)),
            "the global min must survive, never be renamed"
        );
    }

    // --- Mixed group: collapse + empty-drop + keep-both in one resolution ---

    /// A four-file group exercising all three rules at once: two identical (one
    /// collapses), one empty (drops), two distinct non-empty (min survives, other
    /// renamed). The expected outcome is exactly: one collapse for the duplicate, one
    /// collapse for the empty, one rename for the non-survivor of the two remaining.
    #[test]
    fn mixed_group_collapse_drop_and_keep_both() {
        // U1 & U2 identical (hash 7); U3 empty; U4 non-empty distinct (hash 9).
        // After step 1: reps = {U1 (hash 7), U3 empty, U4 (hash 9)} (U2 collapsed).
        // After step 2: U3 empty drops (U1, U4 non-empty present).
        // After step 3: survivor = min(U1, U4) = U1; U4 renamed.
        let ops = resolve_structure(view_at(
            "Note.md",
            vec![file(U1, 7), file(U2, 7), empty_file(U3), file(U4, 9)],
        ));
        assert_eq!(
            collapsed(&ops),
            vec![uuid(U2), uuid(U3)],
            "the duplicate (U2) and the empty (U3) both collapse"
        );
        assert_eq!(
            renamed(&ops),
            vec![(uuid(U4), format!("Note (conflict {}).md", uuid(U4)))],
            "U1 survives; U4 (the other non-empty) is renamed"
        );
    }

    // --- Transitive self-collision resolved within the same fixpoint (INV-5.2) ---

    /// A file-collision group whose loser's conflict path ALREADY holds a live user
    /// file. The rename re-surfaces as a new collision the same fixpoint resolves:
    /// min-UUID of {loser, occupant} wins the conflict path, the other gets a further
    /// `(conflict <uuid>)`. Nothing is dropped — every input UUID survives at some
    /// path or appears as a rename target (INV-3) — the pass terminates, and a
    /// shuffled input yields the identical final placement.
    #[test]
    fn transitive_self_collision_resolves_deterministically() {
        // Group at Note.md: U1 (min, survives), U2 (loser → "Note (conflict U2).md").
        // A pre-existing live file U3 already sits at exactly "Note (conflict U2).md".
        let conflict_path_u2 = format!("Note (conflict {}).md", uuid(U2));
        let mut occupants = BTreeMap::new();
        occupants.insert("Note.md".to_string(), vec![file(U1, 1), file(U2, 2)]);
        occupants.insert(conflict_path_u2.clone(), vec![file(U3, 3)]);
        let view = StructuralView { occupants };

        let ops = resolve_structure(view.clone());
        let final_paths = replay(view.clone(), &ops);

        // Every input UUID is placed somewhere — nothing silently dropped (INV-3).
        let placed: std::collections::BTreeSet<Uuid> = final_paths.values().copied().collect();
        assert_eq!(
            placed,
            [uuid(U1), uuid(U2), uuid(U3)].into_iter().collect(),
            "all three documents survive at some path"
        );

        // U1 keeps Note.md. At the contested conflict path, min(U2, U3) = U2 wins, so
        // U3 is pushed to a further conflict file off U2's conflict path.
        assert_eq!(final_paths.get("Note.md"), Some(&uuid(U1)));
        assert_eq!(
            final_paths.get(&conflict_path_u2),
            Some(&uuid(U2)),
            "min-UUID (U2) wins the contested conflict path"
        );
        let u3_path = conflict_name(&conflict_path_u2, &uuid(U3));
        assert_eq!(
            final_paths.get(&u3_path),
            Some(&uuid(U3)),
            "the occupant (U3) is pushed to a further conflict file"
        );

        // No path ends up doubly-occupied — the fixpoint fully resolved.
        let mut all: Vec<&Uuid> = final_paths.values().collect();
        let count = all.len();
        all.sort();
        all.dedup();
        assert_eq!(
            all.len(),
            count,
            "no path holds two documents after resolution"
        );

        // Shuffle the input occupant order → identical final placement.
        for shuffle in permutations(&[file(U1, 1), file(U2, 2)]) {
            let mut occ = BTreeMap::new();
            occ.insert("Note.md".to_string(), shuffle);
            occ.insert(conflict_path_u2.clone(), vec![file(U3, 3)]);
            let v = StructuralView { occupants: occ };
            let shuffled_ops = resolve_structure(v.clone());
            assert_eq!(
                replay(v, &shuffled_ops),
                final_paths,
                "shuffled input converges to the identical final placement"
            );
        }
    }

    // --- Termination: every valid step strictly decreases the lexicographic measure ---

    /// The P2a termination AC: the resolver completes (never hits the O(N²)
    /// `MAX_STEPS` guard) on a valid view that forces a TRANSITIVE step — a
    /// rename-onto-an-occupied-path. Within the file cascade alone (the lowest
    /// precedence component), such a step is net-zero on `file_collision_sites`: the
    /// resolved group loses its site but the rename surfaces a new one at the
    /// occupied target. Termination there rests on INV-5.2's UUID-unique naming (a
    /// renamed loser never revisits a path, so the cascade is bounded), NOT on a
    /// per-step decrease of the file-collision component — that per-step
    /// lexicographic decrease is the guarantee for the UNIFIED pass *across* rule
    /// families (P2d/P2e), where each rule only ever surfaces a strictly-lower
    /// component. So this test proves the cascade terminates AND lands every input
    /// document somewhere (nothing dropped, INV-3); the `MAX_STEPS` guard remains as
    /// the loud-failure tripwire for a future buggy measure.
    #[test]
    fn fixpoint_terminates_on_transitive_rename_onto_occupied_path() {
        // A 3-way collision whose min-non-survivor (U2) renames onto an occupied
        // conflict path (U4 pre-lives there) — forcing a second, transitive
        // resolution at the conflict path.
        let conflict_path_u2 = format!("Note (conflict {}).md", uuid(U2));
        let mut occupants = BTreeMap::new();
        occupants.insert(
            "Note.md".to_string(),
            vec![file(U1, 1), file(U2, 2), file(U3, 3)],
        );
        occupants.insert(conflict_path_u2, vec![file(U4, 4)]);
        let view = StructuralView { occupants };

        // Precondition: the measure derivation sees the unresolved collision (the
        // 3-file group at Note.md is one file-collision site; the conflict path holds
        // a single file so far, not yet a collision).
        assert_eq!(
            view.measure(),
            Measure {
                revive_sites: 0,
                folder_collision_sites: 0,
                file_vs_folder_sites: 0,
                file_collision_sites: 1,
            },
            "the starting view has exactly one file-collision site to resolve"
        );

        // Returns rather than panicking on the guard — the cascade terminated.
        let ops = resolve_structure(view.clone());
        assert!(!ops.is_empty());

        // Every input document lands somewhere — the transitive resolution dropped
        // nothing (INV-3). U1 keeps Note.md; the rest cascade through conflict files.
        let placement = replay(view, &ops);
        let placed: std::collections::BTreeSet<Uuid> = placement.values().copied().collect();
        assert_eq!(
            placed,
            [uuid(U1), uuid(U2), uuid(U3), uuid(U4)]
                .into_iter()
                .collect(),
            "all four documents survive at distinct paths after the transitive resolution"
        );
        // And no path is doubly-occupied — the fixpoint fully resolved.
        let mut paths: Vec<&String> = placement.keys().collect();
        let total = paths.len();
        paths.sort();
        paths.dedup();
        assert_eq!(
            paths.len(),
            total,
            "no path holds two documents after resolution"
        );
    }

    /// The B1 fix's load-bearing property: the termination measure is compared
    /// **lexicographically** (dictionary order, most-significant component first),
    /// NOT as a flat sum. P2a only populates `file_collision_sites`, but the full
    /// tuple is encoded now so P2d/P2e's higher components order correctly — this
    /// pins that ordering. A more-significant component dominates regardless of the
    /// lower ones, and a flat sum would mis-rank these pairs.
    #[test]
    fn measure_orders_lexicographically_not_by_flat_sum() {
        let m = |revive, folder, fvf, file| Measure {
            revive_sites: revive,
            folder_collision_sites: folder,
            file_vs_folder_sites: fvf,
            file_collision_sites: file,
        };

        // Most-significant component dominates: one revive site outranks any number
        // of lower-component sites (a flat sum would rank the second one higher).
        assert!(m(0, 9, 9, 9) < m(1, 0, 0, 0));
        // Ties on the top component fall through to the next (folder collisions).
        assert!(m(1, 0, 9, 9) < m(1, 1, 0, 0));
        // …and so on down the precedence chain to the file-collision component.
        assert!(m(1, 1, 0, 9) < m(1, 1, 1, 0));
        assert!(m(1, 1, 1, 0) < m(1, 1, 1, 1));
        // The zero tuple is the minimum (a fully-resolved view).
        assert!(m(0, 0, 0, 0) < m(0, 0, 0, 1));
    }

    // --- conflict_name unit cases ---

    /// Conflict naming at the vault root: no parent dir, full UUID embedded, `.md`
    /// extension preserved.
    #[test]
    fn conflict_name_root() {
        let name = conflict_name("Note.md", &uuid(U2));
        assert_eq!(name, format!("Note (conflict {}).md", uuid(U2)));
    }

    /// Conflict naming in a nested directory preserves the full parent path.
    #[test]
    fn conflict_name_nested() {
        let name = conflict_name("a/b/Note.md", &uuid(U2));
        assert_eq!(name, format!("a/b/Note (conflict {}).md", uuid(U2)));
    }

    /// A multi-dot filename keeps everything before the trailing `.md` as the stem
    /// (only the final `.md` is the extension) — `Note.draft.md` → stem `Note.draft`.
    #[test]
    fn conflict_name_multi_dot_stem() {
        let name = conflict_name("Note.draft.md", &uuid(U2));
        assert_eq!(name, format!("Note.draft (conflict {}).md", uuid(U2)));
    }

    // --- test-only helpers ---

    /// Replay a plan over a starting view to its final file placement: a map from
    /// final display path → the surviving file UUID at it. Mirrors how the apply side
    /// (P2b) materializes the plan, so the tests can assert on observable end-state
    /// (which file lands where) rather than on the op sequence.
    ///
    /// Tracks placement **per UUID** (`uuid -> current path`), NOT per path — the
    /// starting view can hold several files at one path, which a path-keyed map would
    /// collapse. A `CollapseFile` removes its loser; a `RenameFile` moves its loser to
    /// the new path. After a correctly-resolved plan every surviving UUID holds a
    /// distinct path, so inverting to path → uuid is lossless.
    fn replay(view: StructuralView, ops: &[StructuralOp]) -> BTreeMap<String, Uuid> {
        let mut by_uuid: BTreeMap<Uuid, String> = BTreeMap::new();
        for (path, nodes) in &view.occupants {
            for node in nodes {
                if let Some(uuid) = node.file_uuid() {
                    by_uuid.insert(uuid, path.clone());
                }
            }
        }
        for op in ops {
            match op {
                StructuralOp::CollapseFile { loser } => {
                    by_uuid.remove(loser);
                }
                StructuralOp::RenameFile { loser, to } => {
                    by_uuid.insert(*loser, to.clone());
                }
                // P2a emits no folder ops; ignore for the replay model.
                _ => {}
            }
        }

        let mut by_path: BTreeMap<String, Uuid> = BTreeMap::new();
        for (uuid, path) in by_uuid {
            assert!(
                by_path.insert(path.clone(), uuid).is_none(),
                "replay invariant: two surviving documents at the same path {path} — \
                 the plan did not fully resolve"
            );
        }
        by_path
    }

    /// All permutations of a small node slice (for input-shuffle determinism checks).
    /// Heap's-algorithm-free recursive enumeration — the slices here are ≤4 elements.
    fn permutations(nodes: &[NodeKind]) -> Vec<Vec<NodeKind>> {
        if nodes.len() <= 1 {
            return vec![nodes.to_vec()];
        }
        let mut out = Vec::new();
        for i in 0..nodes.len() {
            let mut rest = nodes.to_vec();
            let head = rest.remove(i);
            for mut tail in permutations(&rest) {
                tail.insert(0, head);
                out.push(tail);
            }
        }
        out
    }
}
