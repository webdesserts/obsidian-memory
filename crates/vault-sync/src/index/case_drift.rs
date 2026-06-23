//! Case-only-rename detection (Bug 1 / Fix 3 Facet B — the sender side).
//!
//! A folder case-rename (`Plans/ → plans/`) on a case-INSENSITIVE filesystem
//! (APFS) produces NO `Deleted(old)` watcher event — the old casing still
//! resolves on disk, so the watcher's `path.exists()` probe lies and the
//! move-coalescer never sees a delete half to pair. The detection therefore
//! cannot reach the event-driven path; a periodic sweep is the reliable signal.
//!
//! This module is the PURE core of that sweep: it takes a case-SENSITIVE
//! on-disk `.md` path listing and compares it against the (case-sensitive)
//! Index, returning the moves needed to re-home each case-drifted entry. It is
//! fs-free and wall-clock-free — the daemon supplies the disk listing and acts
//! on the returned moves, keeping the same purity discipline as `MoveCoalescer`.
//!
//! ## Folder-aware (load-bearing, not an optimization)
//!
//! A naive sweep emits one per-FILE move for each drifted file. After 198
//! `Plans/* → plans/*` file-moves the index holds a live `plans/` folder AND a
//! live, now-EMPTY `Plans/` folder node (the old folder node was never re-homed),
//! so `Vault::materialize_folders` re-`mkdir`s `Plans/` — which collapses onto
//! `plans/` on APFS and re-broadcasts the case-dup (the 06-22 case-war reborn).
//! So a whole-folder case rename MUST emit ONE `Index::move_subtree` (which
//! re-homes the folder node, leaving no orphan) rather than N per-file moves.
//! Only a bare leaf-FILENAME case change (no folder segment changed) emits a
//! per-file `move_node`.
//!
//! ## ASCII-only case folding
//!
//! Case comparison uses `eq_ignore_ascii_case`. Non-ASCII case-drift simply
//! isn't detected — a pre-existing, safe limitation (the blocked fleet rename is
//! ASCII).

use super::Index;
use std::collections::HashSet;

/// A folder whose casing drifted between the index and the disk — re-homed with
/// one [`Index::move_subtree`], preserving every descendant file UUID.
///
/// `old_prefix` is the folder's path at the index's (stale) casing;
/// `new_prefix` is its casing on disk. They differ only by ASCII case.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FolderCaseMove {
    /// The folder's path at the index's stale casing (the `move_subtree` source).
    pub old_prefix: String,
    /// The folder's path at the disk's casing (the `move_subtree` target).
    pub new_prefix: String,
}

/// A leaf file whose FILENAME casing drifted (its folder did not) — re-homed
/// with one [`Index::move_node`], preserving the file's UUID.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileCaseMove {
    /// The file's path at the index's stale casing (the `move_node` source).
    pub old_path: String,
    /// The file's path at the disk's casing (the `move_node` target).
    pub new_path: String,
}

/// The case-drift a sweep detected: folder-level renames (re-home the subtree)
/// and bare leaf-filename renames (re-home the single file node). Folder moves
/// are deduplicated — many drifted files under one renamed folder yield ONE
/// [`FolderCaseMove`].
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct CaseDrift {
    /// Folder case-renames, applied via `move_subtree` (one per renamed folder).
    pub folder_moves: Vec<FolderCaseMove>,
    /// Leaf-filename case-renames, applied via `move_node` (one per file).
    pub file_moves: Vec<FileCaseMove>,
}

impl CaseDrift {
    /// Whether the sweep found nothing to re-home (the common idle case).
    pub fn is_empty(&self) -> bool {
        self.folder_moves.is_empty() && self.file_moves.is_empty()
    }
}

impl Index {
    /// Compare a case-SENSITIVE on-disk `.md` listing against this (case-sensitive)
    /// index and return the moves needed to converge drifted casing.
    ///
    /// A disk path is "drifted" when it is NOT in the index verbatim, but the index
    /// holds a live node at a path that matches it case-INSENSITIVELY (ASCII), and
    /// the disk's own casing has NO live node. That is exactly a case-only rename
    /// the watcher could not see.
    ///
    /// Drift on a FOLDER segment (a directory component changed case) is collapsed
    /// into one [`FolderCaseMove`] per renamed folder — the shallowest differing
    /// folder segment names the rename — so a 198-file folder rename is one
    /// `move_subtree`, not 198 file moves (the anti-ping-pong requirement). Drift on
    /// only the leaf FILENAME yields a [`FileCaseMove`]. A folder rename is emitted
    /// only when a live folder node exists at the stale prefix (so the `move_subtree`
    /// has a real node to re-home); if not, the file falls back to a leaf move.
    ///
    /// Pure: takes the disk listing, reads the index path/folder caches, mutates
    /// nothing. The daemon applies the returned moves and persists once.
    pub fn detect_case_drift(&self, disk_md_paths: &[String]) -> CaseDrift {
        let index_paths: HashSet<String> = self.path_to_node().keys().cloned().collect();

        let mut folder_moves: Vec<FolderCaseMove> = Vec::new();
        let mut folder_seen: HashSet<(String, String)> = HashSet::new();
        let mut file_moves: Vec<FileCaseMove> = Vec::new();

        for disk_path in disk_md_paths {
            // An exact index match is no drift — the casing already agrees. (This
            // also subsumes a live-node check: the path cache keys ARE exactly the
            // paths a `node_for_path` lookup would resolve, so a disk path with its
            // own live node is necessarily an exact index match and already skipped.)
            if index_paths.contains(disk_path) {
                continue;
            }
            // Find the index path that matches case-insensitively. A genuinely-new
            // lowercase file (no case-matching index node) yields no match → no pair.
            let Some(index_path) = index_paths
                .iter()
                .find(|p| p.eq_ignore_ascii_case(disk_path))
            else {
                continue;
            };

            match Self::case_drift_segment(index_path, disk_path) {
                Some(DriftKind::Folder {
                    old_prefix,
                    new_prefix,
                }) if self.find_folder_node(&old_prefix).is_some() => {
                    if folder_seen.insert((old_prefix.clone(), new_prefix.clone())) {
                        folder_moves.push(FolderCaseMove {
                            old_prefix,
                            new_prefix,
                        });
                    }
                }
                // A folder-level drift with no live folder node at the stale prefix
                // can't be re-homed as a subtree; fall back to a per-file move so the
                // node is still re-pathed.
                Some(DriftKind::Folder { .. }) | Some(DriftKind::Leaf) => {
                    file_moves.push(FileCaseMove {
                        old_path: index_path.clone(),
                        new_path: disk_path.clone(),
                    });
                }
                None => {}
            }
        }

        CaseDrift {
            folder_moves,
            file_moves,
        }
    }

    /// Classify where the case-drift between two case-insensitively-equal paths
    /// lives: the shallowest FOLDER segment that differs (a directory rename), or
    /// the leaf filename alone.
    ///
    /// Both paths are assumed `eq_ignore_ascii_case` and unequal (the caller's
    /// guard). Returns `None` only if the segment counts differ (not a pure case
    /// rename — a structural relocation, which is the watcher/coalescer's job).
    fn case_drift_segment(index_path: &str, disk_path: &str) -> Option<DriftKind> {
        let index_segs: Vec<&str> = index_path.split('/').collect();
        let disk_segs: Vec<&str> = disk_path.split('/').collect();
        if index_segs.len() != disk_segs.len() {
            return None;
        }
        let last = index_segs.len() - 1;
        for (i, (iseg, dseg)) in index_segs.iter().zip(disk_segs.iter()).enumerate() {
            if iseg == dseg {
                continue;
            }
            // A non-case-difference at the same segment count means the paths are
            // not a pure case rename — leave it to the event-driven move path.
            if !iseg.eq_ignore_ascii_case(dseg) {
                return None;
            }
            if i < last {
                // A directory segment changed case: the shallowest such segment
                // names the folder rename (all descendants follow via move_subtree).
                let old_prefix = index_segs[..=i].join("/");
                let new_prefix = disk_segs[..=i].join("/");
                return Some(DriftKind::Folder {
                    old_prefix,
                    new_prefix,
                });
            }
            // Only the leaf filename differs.
            return Some(DriftKind::Leaf);
        }
        None
    }
}

/// Where a case-drift between two paths lives (internal classification).
enum DriftKind {
    /// A directory segment changed case — re-home the whole subtree.
    Folder {
        old_prefix: String,
        new_prefix: String,
    },
    /// Only the leaf filename changed case — re-home the single file node.
    Leaf,
}

#[cfg(test)]
mod tests {
    //! Detector tests feed a case-SENSITIVE disk path set directly — deliberately
    //! NOT through `InMemoryFs`, which is case-sensitive and would mask the bug by
    //! making the old-cased disk path genuinely absent. The whole point of the
    //! detector is to be testable without an fs's case behavior.

    use super::*;
    use uuid::Uuid;

    const FP: [u8; 32] = [0u8; 32];

    fn index_with(paths: &[&str]) -> Index {
        let index = Index::new(1);
        for (i, p) in paths.iter().enumerate() {
            index
                .register_document(p, &Uuid::from_u128(i as u128 + 1), &FP)
                .expect("register");
        }
        index
    }

    fn disk(paths: &[&str]) -> Vec<String> {
        paths.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn folder_case_rename_emits_one_folder_move() {
        // Index holds `Plans/a.md` + `Plans/b.md`; disk now reads `plans/`.
        let index = index_with(&["Plans/a.md", "Plans/b.md"]);
        let drift = index.detect_case_drift(&disk(&["plans/a.md", "plans/b.md"]));

        assert_eq!(
            drift.folder_moves,
            vec![FolderCaseMove {
                old_prefix: "Plans".to_string(),
                new_prefix: "plans".to_string(),
            }],
            "two drifted files under one renamed folder collapse to ONE folder move"
        );
        assert!(
            drift.file_moves.is_empty(),
            "a folder rename emits no per-file moves"
        );
    }

    #[test]
    fn nested_folder_case_rename_uses_shallowest_segment() {
        // Only the top segment changed case; the rename is named by it.
        let index = index_with(&["Plans/sub/a.md"]);
        let drift = index.detect_case_drift(&disk(&["plans/sub/a.md"]));

        assert_eq!(
            drift.folder_moves,
            vec![FolderCaseMove {
                old_prefix: "Plans".to_string(),
                new_prefix: "plans".to_string(),
            }],
            "the shallowest differing folder segment names the subtree move"
        );
    }

    #[test]
    fn leaf_filename_case_rename_emits_a_file_move_not_a_folder_move() {
        // Same folder casing; only the filename changed case.
        let index = index_with(&["a/Foo.md"]);
        let drift = index.detect_case_drift(&disk(&["a/foo.md"]));

        assert!(
            drift.folder_moves.is_empty(),
            "a bare filename case change is NOT a folder rename"
        );
        assert_eq!(
            drift.file_moves,
            vec![FileCaseMove {
                old_path: "a/Foo.md".to_string(),
                new_path: "a/foo.md".to_string(),
            }]
        );
    }

    #[test]
    fn unchanged_disk_listing_returns_no_drift() {
        let index = index_with(&["Plans/a.md", "notes/b.md"]);
        let drift = index.detect_case_drift(&disk(&["Plans/a.md", "notes/b.md"]));
        assert!(drift.is_empty(), "matching casing is not drift");
    }

    #[test]
    fn genuinely_new_lowercase_file_is_not_drift() {
        // No case-matching index node → a real create, not a case-rename.
        let index = index_with(&["Plans/a.md"]);
        let drift = index.detect_case_drift(&disk(&["Plans/a.md", "brandnew.md"]));
        assert!(
            drift.is_empty(),
            "a file with no case-insensitive index twin is a create, not drift"
        );
    }

    #[test]
    fn disk_path_with_its_own_live_node_is_not_drift() {
        // Both `Plans/a.md` AND `plans/a.md` already have live nodes (two genuine
        // files differing only in case) — neither is a rename of the other.
        let index = index_with(&["Plans/a.md", "plans/a.md"]);
        let drift = index.detect_case_drift(&disk(&["Plans/a.md", "plans/a.md"]));
        assert!(
            drift.is_empty(),
            "a disk path that already has its own node is not a rename target"
        );
    }
}
