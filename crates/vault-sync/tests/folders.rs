//! Folder-structural acceptance tests — the folder-safe move primitive
//! `Index::move_subtree` (INV-1.5b).
//!
//! A folder move is ONE `tree.mov` on the folder node; loro carries the
//! descendants structurally for free, so the document content never moves and
//! every descendant keeps its UUID. The primitive's job is to keep the
//! denormalized per-file `path` meta + the path↔node caches correct after that
//! single re-parent — the per-file `move_node` does not (it touches only the one
//! node it moves). These tests pin: descendant UUID preservation + zero content
//! re-transfer across a sync, the descendant `path`-meta rewrite (load-bearing for
//! the deleted-paths guard), and the folder-safe-vs-raw-`move_node` cache contrast.
//!
//! Everything runs against `InMemoryFs` — no test touches a real vault. The
//! replica/handshake/edit helpers live in the shared [`common`] harness.

mod common;
use common::*;

use vault_sync::{FileSystem, InMemoryFs, IndexError, Vault, conflict_name, content_doc_path};

use std::collections::BTreeSet;
use std::sync::Arc;
use uuid::Uuid;

// ========================= AC-INV-1.5b — folder move (zero content) =========================

mod ac_inv_1_5b_folder_move {
    use super::*;

    /// A folder move preserves every descendant's UUID and re-transfers ZERO
    /// document content. A builds a `proj/` folder with two top-level files and a
    /// sub-folder file, syncs to B, then re-parents the whole folder under
    /// `archive/`. The next sync carries only the structural `tree.mov` — no
    /// document-content bytes — and B converges with every descendant at its new
    /// `archive/proj/...` path under its original UUID (the folder analogue of
    /// AC-INV-1).
    #[tokio::test]
    async fn folder_move_preserves_descendant_uuids_with_zero_content() {
        let (a, b, fs_a, fs_b) = two_vaults().await;

        // A builds a folder with two files plus a sub-folder file, and both
        // replicas converge on the whole subtree.
        write_and_index(&a, &fs_a, "proj/a.md", "# A\n\nAlpha body.").await;
        write_and_index(&a, &fs_a, "proj/b.md", "# B\n\nBeta body.").await;
        write_and_index(&a, &fs_a, "proj/sub/c.md", "# C\n\nGamma body.").await;
        full_sync(&a, &b).await;

        // Capture the pre-move UUIDs on BOTH sides — identity must survive the move.
        let uuid_a = uuid_at(&a, "proj/a.md");
        let uuid_b = uuid_at(&a, "proj/b.md");
        let uuid_c = uuid_at(&a, "proj/sub/c.md");
        assert_eq!(uuid_at(&b, "proj/a.md"), uuid_a, "B shares a.md's UUID");
        assert_eq!(uuid_at(&b, "proj/b.md"), uuid_b, "B shares b.md's UUID");
        assert_eq!(uuid_at(&b, "proj/sub/c.md"), uuid_c, "B shares c.md's UUID");

        // A re-parents the entire folder under `archive/` — ONE structural op.
        move_subtree(&a, &fs_a, "proj", "archive/proj").await;

        // Sync the folder move under a byte counter and assert zero content crossed.
        let mut counter = ByteCounter::new();
        counter.full_sync_counting(&a, &b).await;
        assert_eq!(
            counter.total_document_content_bytes(),
            0,
            "a folder move ships only the structural tree.mov — zero document content"
        );

        // Every descendant moved to its new path on B, each keeping its UUID.
        assert!(
            b.index().node_for_path("proj/a.md").is_none(),
            "B's old folder path is vacated"
        );
        assert_eq!(
            uuid_at(&b, "archive/proj/a.md"),
            uuid_a,
            "a.md keeps its UUID at the new path on B"
        );
        assert_eq!(
            uuid_at(&b, "archive/proj/b.md"),
            uuid_b,
            "b.md keeps its UUID at the new path on B"
        );
        assert_eq!(
            uuid_at(&b, "archive/proj/sub/c.md"),
            uuid_c,
            "the sub-folder file keeps its UUID at the new path on B"
        );

        // And the UUIDs are equally stable on A (identity is move-stable, not just
        // convergent).
        assert_eq!(uuid_at(&a, "archive/proj/a.md"), uuid_a);
        assert_eq!(uuid_at(&a, "archive/proj/b.md"), uuid_b);
        assert_eq!(uuid_at(&a, "archive/proj/sub/c.md"), uuid_c);

        // The content `.loro`s were never relocated (addressed by stable UUID), so
        // the same files back the documents, and each `.md` re-materialized at its
        // NEW path from its path-independent `.loro` (zero content crossed the wire).
        for uuid in [uuid_a, uuid_b, uuid_c] {
            assert!(
                fs_b.exists(&content_doc_path(&uuid)).await.unwrap(),
                "B still has the same <uuid>.loro after the folder move"
            );
        }
        assert!(!fs_b.exists("proj/a.md").await.unwrap());
        assert!(!fs_b.exists("proj/sub/c.md").await.unwrap());
        assert!(fs_b.exists("archive/proj/a.md").await.unwrap());
        assert!(fs_b.exists("archive/proj/b.md").await.unwrap());
        assert!(fs_b.exists("archive/proj/sub/c.md").await.unwrap());
    }

    /// After a folder move, a descendant's `path` meta reads its NEW path — proven
    /// by deleting it and getting a clean tombstone. The denormalized `path` meta is
    /// what the deleted-paths guard re-derives from for a tombstoned node (its tree
    /// position is no longer walkable), so a stale `proj/a.md` meta left behind by a
    /// folder-unaware move would arm the guard at the WRONG path. A clean tombstone
    /// at the new path is the observable that the descendant rewrite landed.
    #[tokio::test]
    async fn folder_move_rewrites_descendant_path_meta() {
        let (a, fs_a) = one_vault().await;

        write_and_index(&a, &fs_a, "proj/a.md", "# A\n\nAlpha body.").await;
        write_and_index(&a, &fs_a, "proj/sub/c.md", "# C\n\nGamma body.").await;

        move_subtree(&a, &fs_a, "proj", "archive/proj").await;

        // Deleting the moved descendant at its NEW path tombstones a live node and
        // arms the deleted-paths guard for that exact path.
        let tombstoned = a.index().delete_node("archive/proj/a.md").unwrap();
        assert!(
            tombstoned,
            "the descendant resolves at its new path — delete_node finds and tombstones it"
        );
        assert!(
            a.index().is_path_deleted("archive/proj/a.md"),
            "the deleted-paths guard is armed at the NEW descendant path"
        );

        // The stale OLD path must NOT be what the guard recorded — a folder-unaware
        // move that left `proj/a.md` in the meta would mis-fire the guard here.
        assert!(
            !a.index().is_path_deleted("proj/a.md"),
            "the guard is NOT armed at the stale old descendant path"
        );

        // The rewrite survives a cache rebuild from index truth: rebuilding re-derives
        // the deleted-paths set from each tombstoned node's `path` meta, so the new
        // path stays guarded and the old one stays clear only if the meta was rewritten.
        a.index().rebuild_caches();
        assert!(
            a.index().is_path_deleted("archive/proj/a.md"),
            "after a rebuild, the tombstoned descendant's path meta still reads the NEW path"
        );
        assert!(!a.index().is_path_deleted("proj/a.md"));
    }

    /// The folder-safe contrast a raw `move_node` on the folder gets wrong: after
    /// `move_subtree`, a descendant resolves through the cache at its NEW path and no
    /// longer at the old one. A raw `move_node` would re-parent the folder node but
    /// leave every descendant's `path_to_node` entry pointing at the stale old path.
    #[tokio::test]
    async fn folder_move_repoints_descendant_caches() {
        let (a, fs_a) = one_vault().await;

        write_and_index(&a, &fs_a, "proj/a.md", "# A\n\nAlpha body.").await;
        write_and_index(&a, &fs_a, "proj/sub/c.md", "# C\n\nGamma body.").await;

        move_subtree(&a, &fs_a, "proj", "archive/proj").await;

        // Top-level descendant: resolves at the new path, vacated at the old.
        assert!(
            a.index().node_for_path("archive/proj/a.md").is_some(),
            "the descendant resolves at its new path after move_subtree"
        );
        assert!(
            a.index().node_for_path("proj/a.md").is_none(),
            "the descendant no longer resolves at its old path (the raw-move_node bug)"
        );

        // Sub-folder descendant: the rewrite reaches arbitrary depth.
        assert!(
            a.index().node_for_path("archive/proj/sub/c.md").is_some(),
            "a deeper descendant also resolves at its new path"
        );
        assert!(
            a.index().node_for_path("proj/sub/c.md").is_none(),
            "the deeper descendant no longer resolves at its old path"
        );
    }
}

// ===================== move_subtree edges — clean structural mover =====================

/// `move_subtree` is a clean STRUCTURAL mover: it refuses an occupied target rather
/// than merging, errors on a missing source, and no-ops on an identical move.
/// Collision POLICY (folder-merge, file-vs-folder) lives downstream in the conflict
/// resolver (P2d) — these pin that the primitive itself stays policy-free.
mod move_subtree_edges {
    use super::*;

    /// A target already occupied by a FOLDER is folder-MERGE territory the resolver
    /// owns — the raw primitive errors with `MoveTargetExists` and does NOT merge.
    #[tokio::test]
    async fn errors_when_target_is_an_existing_folder() {
        let (a, fs_a) = one_vault().await;
        // Two distinct folders, each with a file, so both folder nodes exist.
        write_and_index(&a, &fs_a, "proj/a.md", "alpha").await;
        write_and_index(&a, &fs_a, "archive/keep.md", "keep").await;

        let err = a.index().move_subtree("proj", "archive").unwrap_err();
        assert!(
            matches!(err, IndexError::MoveTargetExists(_)),
            "moving onto an occupied folder is a MoveTargetExists, got {err:?}"
        );
        // The source folder is untouched — the refused move left it in place.
        assert!(a.index().node_for_path("proj/a.md").is_some());
        assert!(a.index().node_for_path("archive/keep.md").is_some());
    }

    /// A target occupied by a FILE is file-vs-folder territory the resolver owns —
    /// again a `MoveTargetExists`, not a clobber. (Index file paths end in `.md`, so
    /// the file-collision case is a folder whose new prefix coincides with an existing
    /// file's path — the `find_node_by_path(new_prefix)` branch.)
    #[tokio::test]
    async fn errors_when_target_is_an_existing_file() {
        let (a, fs_a) = one_vault().await;
        write_and_index(&a, &fs_a, "proj/a.md", "alpha").await;
        // A FILE already lives at the exact path the folder would move to.
        write_and_index(&a, &fs_a, "notes/topic.md", "occupant").await;

        let err = a
            .index()
            .move_subtree("proj", "notes/topic.md")
            .unwrap_err();
        assert!(
            matches!(err, IndexError::MoveTargetExists(_)),
            "moving onto an occupied file path is a MoveTargetExists, got {err:?}"
        );
        assert!(
            a.index().node_for_path("notes/topic.md").is_some(),
            "the occupying file is untouched by the refused move"
        );
    }

    /// No folder node at the source prefix → `MoveSourceMissing` (nothing to move).
    #[tokio::test]
    async fn errors_when_source_folder_is_missing() {
        let (a, _fs_a) = one_vault().await;
        let err = a
            .index()
            .move_subtree("ghost", "archive/ghost")
            .unwrap_err();
        assert!(
            matches!(err, IndexError::MoveSourceMissing(_)),
            "a missing source folder is a MoveSourceMissing, got {err:?}"
        );
    }

    /// Moving a folder onto itself is a no-op — identity-prefix returns Ok with the
    /// subtree unchanged (mirrors `move_node`'s identical-path no-op).
    #[tokio::test]
    async fn identical_prefix_is_a_noop() {
        let (a, fs_a) = one_vault().await;
        write_and_index(&a, &fs_a, "proj/a.md", "alpha").await;
        let uuid_before = uuid_at(&a, "proj/a.md");

        a.index().move_subtree("proj", "proj").unwrap();

        assert_eq!(
            uuid_at(&a, "proj/a.md"),
            uuid_before,
            "an identical-prefix move leaves the subtree untouched"
        );
    }
}

// ===================== shared assertion helpers (folder cascade) =====================

/// The set of `.md` files a vault currently materializes on disk (survivors + conflict
/// files + relocated files), for exact-set assertions.
async fn md_files(vault: &V) -> BTreeSet<String> {
    vault.list_files().await.unwrap().into_iter().collect()
}

/// The conflict-file path the file cascade renames a loser to (full-UUID suffix).
fn conflict_path(original: &str, loser: &Uuid) -> String {
    conflict_name(original, loser)
}

/// Read a `.md` file as a String (panics if absent).
async fn read_md_str(fs: &Fs, path: &str) -> String {
    String::from_utf8(read_md(fs, path).await).unwrap()
}

/// Count the ALIVE folder nodes whose display path equals `path` in `vault`'s Index —
/// used to assert a folder collision merged down to exactly one surviving folder node
/// (folders aren't materialized in `list_files`, so the merge is observed via the Index
/// scan that the resolver itself reads).
fn folder_nodes_at(vault: &V, path: &str) -> usize {
    vault
        .index()
        .scan_structural_nodes()
        .iter()
        .filter(
            |n| matches!(n, vault_sync::index::StructuralNode::Folder { path: p, .. } if p == path),
        )
        .count()
}

/// Build three empty in-memory vaults (A/B/C, authored 1/2/3) with their retained
/// filesystems — for the ≥3-replica determinism checks.
async fn three_vaults() -> (V, V, V, Fs, Fs, Fs) {
    let fs_a = Arc::new(InMemoryFs::new());
    let fs_b = Arc::new(InMemoryFs::new());
    let fs_c = Arc::new(InMemoryFs::new());
    let a = Vault::init(Arc::clone(&fs_a), author(1)).await.unwrap();
    let b = Vault::init(Arc::clone(&fs_b), author(2)).await.unwrap();
    let c = Vault::init(Arc::clone(&fs_c), author(3)).await.unwrap();
    (a, b, c, fs_a, fs_b, fs_c)
}

// ===================== AC-OQ5-FOLDERMERGE — folder-collision merge =====================
//
// Two replicas independently create distinct FOLDER nodes at one display path (each
// inserts its own `proj/` folder node when it indexes a file under `proj/`). On sync the
// two folder nodes collide; the resolver merges them into the min-TreeID survivor and
// unions their children — and any same-name file the union surfaces falls to the file
// cascade. Checked in BOTH sync directions (the merge is symmetric — no locality).

mod ac_oq5_folder_merge {
    use super::*;

    /// The full folder-merge AC: two replicas each build a distinct `proj/` folder with
    /// a SAME-named `proj/Notes.md` (different content) plus a distinct file each
    /// (`proj/a.md` on A, `proj/b.md` on B) and a SAME-named nested sub-folder
    /// (`proj/sub/` each, a file inside). After converging, every replica agrees on:
    /// a single survivor folder holding the UNION (`proj/a.md`, `proj/b.md`, the resolved
    /// `proj/Notes.md` + its conflict file, `proj/sub/...`), and the surfaced
    /// `proj/Notes.md` collision resolves by the cascade (min-UUID survivor + a full-UUID
    /// conflict file), nothing lost (INV-3). The nested same-name sub-folder merges
    /// transitively. Run BOTH directions — the outcome must be identical.
    #[tokio::test]
    async fn distinct_folders_merge_union_children_surface_file_falls_to_cascade() {
        for direction in ["a_first", "b_first"] {
            let (a, b, fs_a, fs_b) = two_vaults().await;

            // A's `proj/` folder: a distinct file, a same-named Notes.md, a nested file.
            write_and_index(&a, &fs_a, "proj/a.md", "# A\n\nAlpha.\n").await;
            write_and_index(&a, &fs_a, "proj/Notes.md", "# Notes A\n\nFrom A.\n").await;
            write_and_index(&a, &fs_a, "proj/sub/deep.md", "# Deep A\n\nA deep.\n").await;
            // B's `proj/` folder: a distinct file, a same-named Notes.md (DIFFERENT
            // content), a nested file under a same-named sub-folder.
            write_and_index(&b, &fs_b, "proj/b.md", "# B\n\nBeta.\n").await;
            write_and_index(&b, &fs_b, "proj/Notes.md", "# Notes B\n\nFrom B.\n").await;
            write_and_index(&b, &fs_b, "proj/sub/deeper.md", "# Deeper B\n\nB deep.\n").await;

            // The two Notes.md docs collide; min-UUID wins the path, the loser gets a
            // conflict file. (a.md/b.md/deep.md/deeper.md are distinct paths — no
            // collision; they just union under the one survivor folder.)
            let notes_a = uuid_at(&a, "proj/Notes.md");
            let notes_b = uuid_at(&b, "proj/Notes.md");
            let notes_survivor = notes_a.min(notes_b);
            let notes_loser = notes_a.max(notes_b);

            match direction {
                "a_first" => sync_both_ways(&a, &b).await,
                _ => sync_both_ways(&b, &a).await,
            }

            let notes_conflict = conflict_path("proj/Notes.md", &notes_loser);
            // The exact union of `.md` files on both replicas: the four distinct files,
            // the surviving Notes.md, and the one conflict file — nothing stray, nothing
            // lost. (A stray duplicate folder would surface its children at a divergent
            // path; the exact-set assertion is the no-instrument proof the merge unioned
            // under ONE survivor folder.)
            let expected = BTreeSet::from([
                "proj/a.md".to_string(),
                "proj/b.md".to_string(),
                "proj/Notes.md".to_string(),
                notes_conflict.clone(),
                "proj/sub/deep.md".to_string(),
                "proj/sub/deeper.md".to_string(),
            ]);
            assert_eq!(
                md_files(&a).await,
                expected,
                "[{direction}] A: the union under one survivor folder + the Notes conflict file"
            );
            assert_eq!(
                md_files(&b).await,
                expected,
                "[{direction}] B: same exact set (converged, both directions)"
            );

            // The Notes.md collision resolved deterministically (min-UUID survivor).
            assert_eq!(
                uuid_at(&a, "proj/Notes.md"),
                notes_survivor,
                "[{direction}] A: min-UUID Notes survivor"
            );
            assert_eq!(
                uuid_at(&b, "proj/Notes.md"),
                notes_survivor,
                "[{direction}] B: min-UUID Notes survivor"
            );

            // Both Notes bodies survive (INV-3): the survivor's at the path, the loser's
            // at the conflict file, on both replicas.
            let want_survivor = if notes_survivor == notes_a {
                "From A."
            } else {
                "From B."
            };
            let want_loser = if notes_loser == notes_a {
                "From A."
            } else {
                "From B."
            };
            for (label, fs) in [("A", &fs_a), ("B", &fs_b)] {
                assert!(
                    read_md_str(fs, "proj/Notes.md")
                        .await
                        .contains(want_survivor),
                    "[{direction}] {label}: survivor Notes body at the path"
                );
                assert!(
                    read_md_str(fs, &notes_conflict).await.contains(want_loser),
                    "[{direction}] {label}: loser Notes body at the conflict file"
                );
            }

            // Exactly one `proj` folder node survives the merge — the duplicate is gone.
            // (Folders aren't in list_files; assert via the index scan: the survivor + the
            // sub-folder, no duplicate `proj`.)
            for (label, vault) in [("A", &a), ("B", &b)] {
                let proj_folders = folder_nodes_at(vault, "proj");
                assert_eq!(
                    proj_folders, 1,
                    "[{direction}] {label}: exactly one `proj` folder node after the merge"
                );
                let sub_folders = folder_nodes_at(vault, "proj/sub");
                assert_eq!(
                    sub_folders, 1,
                    "[{direction}] {label}: the nested `proj/sub` merged transitively to one node"
                );
            }
        }
    }

    /// A tombstoned child of the LOSER folder stays tombstoned across the merge — the
    /// folder-merge resurrects no deleted note (INV-3 / EC-7). B (the higher-peer
    /// replica, so its `proj/` folder loses to A's min-TreeID survivor) deletes
    /// `proj/gone.md` before syncing; after the merge `proj/gone.md` does NOT come back,
    /// while B's alive `proj/keep_b.md` unions in. (`merge_folder_into` carries a
    /// defensive `is_node_deleted` skip, but loro's `tree.children` already excludes a
    /// tombstoned node — so this pins the user-facing "deleted note stays deleted"
    /// guarantee rather than a load-bearing branch.)
    #[tokio::test]
    async fn tombstoned_child_of_loser_folder_stays_deleted() {
        let (a, b, fs_a, fs_b) = two_vaults().await;

        // A's `proj/` (peer author(1) → smaller TreeID → the survivor).
        write_and_index(&a, &fs_a, "proj/keep_a.md", "# A\n\nKeep A.\n").await;
        // B's `proj/` (peer author(2) → larger TreeID → the loser) with a doomed child.
        write_and_index(&b, &fs_b, "proj/keep_b.md", "# B\n\nKeep B.\n").await;
        write_and_index(&b, &fs_b, "proj/gone.md", "# Gone\n\nDeleted on B.\n").await;
        // B deletes `proj/gone.md` (tombstone) before the merge.
        b.index().delete_node("proj/gone.md").unwrap();
        fs_b.delete("proj/gone.md").await.unwrap();
        b.save_index().await.unwrap();

        sync_both_ways(&a, &b).await;

        // The two `proj` folders merged to one; both alive files unioned in.
        let expected = BTreeSet::from(["proj/keep_a.md".to_string(), "proj/keep_b.md".to_string()]);
        for (label, vault) in [("A", &a), ("B", &b)] {
            assert_eq!(
                md_files(vault).await,
                expected,
                "{label}: the merge unions the alive children and resurrects nothing"
            );
            assert_eq!(
                folder_nodes_at(vault, "proj"),
                1,
                "{label}: one survivor `proj` folder"
            );
            // The deleted note is NOT resurrected by the merge (INV-3).
            assert!(
                vault.index().node_for_path("proj/gone.md").is_none(),
                "{label}: the tombstoned child stays deleted across the folder merge"
            );
        }
        assert!(!fs_a.exists("proj/gone.md").await.unwrap());
        assert!(!fs_b.exists("proj/gone.md").await.unwrap());
    }
}

// ===================== AC-INV-1.5d — file-vs-folder (relocate inside) =====================
//
// A file node and a folder node collide at one display path. The folder wins the path;
// the file relocates INSIDE it at `<folder>/<filename>`, UUID + content preserved, zero
// loss (INV-1.5d, DECIDED relocate-inside). To construct the shape, a folder is named to
// coincide with a file — files are `.md`, so the folder segment ends in `.md`.

mod ac_inv_1_5d_file_vs_folder {
    use super::*;

    /// A creates a FOLDER `Notes.md/` (by indexing `Notes.md/x.md`); B creates a FILE
    /// `Notes.md`. After they converge, the folder keeps `Notes.md`, and B's file
    /// relocates to `Notes.md/Notes.md` — its UUID and body preserved, both present, on
    /// both replicas and in both directions.
    #[tokio::test]
    async fn folder_wins_path_file_relocates_inside() {
        for direction in ["a_first", "b_first"] {
            let (a, b, fs_a, fs_b) = two_vaults().await;

            // A: a folder named `Notes.md` holding a child file.
            write_and_index(&a, &fs_a, "Notes.md/x.md", "# Inside\n\nFolder child.\n").await;
            // B: a real file at `Notes.md`.
            write_and_index(&b, &fs_b, "Notes.md", "# File\n\nA real note.\n").await;
            let file_uuid = uuid_at(&b, "Notes.md");
            let child_uuid = uuid_at(&a, "Notes.md/x.md");

            match direction {
                "a_first" => sync_both_ways(&a, &b).await,
                _ => sync_both_ways(&b, &a).await,
            }

            // The folder wins `Notes.md`: its child still lives there, with its UUID.
            for (label, vault) in [("A", &a), ("B", &b)] {
                assert_eq!(
                    uuid_at(vault, "Notes.md/x.md"),
                    child_uuid,
                    "[{direction}] {label}: the folder's child keeps its path + UUID"
                );
                // The relocated file lives INSIDE the folder, same UUID.
                assert_eq!(
                    uuid_at(vault, "Notes.md/Notes.md"),
                    file_uuid,
                    "[{direction}] {label}: B's file relocated to <folder>/<filename>, UUID preserved"
                );
            }

            // Exactly the two files materialize — the folder child + the relocated file
            // — on both replicas. (`Notes.md` is now a directory, not a `.md` file.)
            let expected =
                BTreeSet::from(["Notes.md/x.md".to_string(), "Notes.md/Notes.md".to_string()]);
            assert_eq!(md_files(&a).await, expected, "[{direction}] A's file set");
            assert_eq!(md_files(&b).await, expected, "[{direction}] B's file set");

            // Both bodies survive (INV-3): the child's and the relocated file's.
            for (label, fs) in [("A", &fs_a), ("B", &fs_b)] {
                assert!(
                    read_md_str(fs, "Notes.md/x.md")
                        .await
                        .contains("Folder child."),
                    "[{direction}] {label}: the folder child's body survived"
                );
                assert!(
                    read_md_str(fs, "Notes.md/Notes.md")
                        .await
                        .contains("A real note."),
                    "[{direction}] {label}: the relocated file's body survived inside the folder"
                );
            }
        }
    }

    /// File-vs-folder whose relocation target is ALREADY occupied: A's folder `Notes.md/`
    /// already holds a live `Notes.md/Notes.md`, and B creates a file `Notes.md` that
    /// would relocate onto exactly that path. The relocate surfaces a file collision the
    /// same pass resolves — min-UUID wins `Notes.md/Notes.md`, the other gets a conflict
    /// file — nothing lost.
    #[tokio::test]
    async fn relocation_onto_occupied_target_falls_to_cascade() {
        let (a, b, fs_a, fs_b) = two_vaults().await;

        // A: a folder `Notes.md/` that already holds a live file at the relocation
        // target `Notes.md/Notes.md`.
        write_and_index(
            &a,
            &fs_a,
            "Notes.md/Notes.md",
            "# Occupant\n\nAlready here.\n",
        )
        .await;
        let occupant = uuid_at(&a, "Notes.md/Notes.md");
        // B: a file at `Notes.md` that will relocate INTO the folder, onto the occupant.
        write_and_index(&b, &fs_b, "Notes.md", "# Incoming\n\nRelocating in.\n").await;
        let incoming = uuid_at(&b, "Notes.md");

        sync_both_ways(&a, &b).await;

        // The relocation target `Notes.md/Notes.md` resolves by min-UUID; the other gets
        // a conflict file off it.
        let target_winner = occupant.min(incoming);
        let target_loser = occupant.max(incoming);
        let further = conflict_path("Notes.md/Notes.md", &target_loser);

        for (label, vault) in [("A", &a), ("B", &b)] {
            assert_eq!(
                uuid_at(vault, "Notes.md/Notes.md"),
                target_winner,
                "[{label}] min-UUID wins the relocation target"
            );
            assert_eq!(
                uuid_at(vault, &further),
                target_loser,
                "[{label}] the other lands at a conflict file off the target"
            );
        }

        // Both documents survive (INV-3): occupant + incoming, at distinct paths.
        let expected = BTreeSet::from(["Notes.md/Notes.md".to_string(), further.clone()]);
        assert_eq!(
            md_files(&a).await,
            expected,
            "A: occupant + relocated-loser conflict"
        );
        assert_eq!(md_files(&b).await, expected, "B: same exact set");
        for (label, fs) in [("A", &fs_a), ("B", &fs_b)] {
            let bodies = format!(
                "{}\n{}",
                read_md_str(fs, "Notes.md/Notes.md").await,
                read_md_str(fs, &further).await,
            );
            assert!(
                bodies.contains("Already here."),
                "[{label}] occupant body survived"
            );
            assert!(
                bodies.contains("Relocating in."),
                "[{label}] incoming body survived"
            );
        }
    }
}

// ===================== determinism — ≥3 replicas, any order =====================

mod folder_cascade_determinism {
    use super::*;

    /// The folder-merge composition converges to the IDENTICAL materialized state across
    /// THREE replicas pumped to quiescence in arbitrary pair order — the cross-replica
    /// determinism guarantee (INV-5.3c) for the folder rules. Three replicas each build a
    /// distinct `proj/` folder with a same-named `proj/Shared.md` (different content) and
    /// a distinct file each; the global-min-TreeID folder survives, the three Shared.md
    /// docs resolve to one survivor + two conflict files, identical everywhere.
    #[tokio::test]
    async fn three_replicas_folder_merge_converges_identically() {
        let (a, b, c, fs_a, fs_b, fs_c) = three_vaults().await;

        write_and_index(&a, &fs_a, "proj/a.md", "# A\n\nAlpha.\n").await;
        write_and_index(&a, &fs_a, "proj/Shared.md", "# Shared A\n\nA.\n").await;
        write_and_index(&b, &fs_b, "proj/b.md", "# B\n\nBeta.\n").await;
        write_and_index(&b, &fs_b, "proj/Shared.md", "# Shared B\n\nB.\n").await;
        write_and_index(&c, &fs_c, "proj/c.md", "# C\n\nGamma.\n").await;
        write_and_index(&c, &fs_c, "proj/Shared.md", "# Shared C\n\nC.\n").await;

        let s_a = uuid_at(&a, "proj/Shared.md");
        let s_b = uuid_at(&b, "proj/Shared.md");
        let s_c = uuid_at(&c, "proj/Shared.md");
        let survivor = s_a.min(s_b).min(s_c);
        let losers: Vec<Uuid> = [s_a, s_b, s_c]
            .into_iter()
            .filter(|u| *u != survivor)
            .collect();

        pump_to_quiescence(&[&a, &b, &c]).await;

        // Every replica: the three distinct files, the surviving Shared.md, and a conflict
        // file per Shared loser — all under one survivor folder, identical everywhere.
        let mut expected = BTreeSet::from([
            "proj/a.md".to_string(),
            "proj/b.md".to_string(),
            "proj/c.md".to_string(),
            "proj/Shared.md".to_string(),
        ]);
        for loser in &losers {
            expected.insert(conflict_path("proj/Shared.md", loser));
        }
        for (label, vault) in [("A", &a), ("B", &b), ("C", &c)] {
            assert_eq!(
                md_files(vault).await,
                expected,
                "{label}: the converged union + Shared conflict files"
            );
            assert_eq!(
                uuid_at(vault, "proj/Shared.md"),
                survivor,
                "{label}: global-min Shared survivor"
            );
            assert_eq!(
                folder_nodes_at(vault, "proj"),
                1,
                "{label}: exactly one `proj` folder node (the others merged away)"
            );
        }

        // No body lost: all three Shared bodies present on A.
        let mut bodies = String::new();
        for path in md_files(&a).await {
            bodies.push_str(&read_md_str(&fs_a, &path).await);
            bodies.push('\n');
        }
        for needle in ["A.", "B.", "C."] {
            assert!(
                bodies.contains(needle),
                "the Shared {needle} body survived (INV-3)"
            );
        }
    }
}

// ===================== AC-INV-1.5a — empty folders sync + materialize =====================
//
// A first-class empty folder node syncs to peers and materializes as a real empty
// DIRECTORY (not merely a side effect of writing files into it), and deleting it removes
// that directory — but ONLY when the directory is empty (a tombstoned folder whose
// directory still holds anything is left in place, never recursively removed: INV-3).
// Folders are invisible to `list_files`/the caches, so these behaviours are observed via
// the on-disk directory state that `materialize_folders` drives.

mod ac_inv_1_5a_empty_folder_materialize {
    use super::*;

    /// Create an empty folder node on `vault` at `path` (no files inside) and flush the
    /// Index — the catalog-level "make an empty directory" the daemon would drive in P4,
    /// returning the folder node's `TreeID` so the test can later delete it by id.
    async fn create_empty_folder(vault: &V, path: &str) -> loro::TreeID {
        let id = vault.index().create_folder(path).unwrap();
        vault.save_index().await.unwrap();
        id
    }

    /// Tombstone an empty folder node by its `TreeID` and flush the Index — the catalog
    /// half of "remove the empty directory". Keyed by id because folders aren't in the
    /// path cache (`delete_node` resolves only file nodes).
    async fn delete_folder(vault: &V, id: loro::TreeID, path: &str) {
        vault.index().delete_node_by_id(id, path).unwrap();
        vault.save_index().await.unwrap();
    }

    /// Whether a directory exists on disk and is empty (the user-visible materialized
    /// state for an empty folder). A non-existent path is reported as absent.
    async fn empty_dir_exists(fs: &Fs, path: &str) -> bool {
        match fs.list(path).await {
            Ok(entries) => entries.is_empty(),
            Err(_) => false,
        }
    }

    /// An empty folder created on A syncs to B and materializes as an empty directory on
    /// B's disk; deleting it on A removes the directory on B. The whole lifecycle carries
    /// only the structural folder node — no document content — and is observed via B's
    /// on-disk directory state (an empty folder has no `.md` to track) plus the Index
    /// folder-node count.
    #[tokio::test]
    async fn empty_folder_syncs_and_materializes_then_removes() {
        let (a, b, _fs_a, fs_b) = two_vaults().await;

        // A creates an empty `notes/` folder node (no files under it).
        let id_a = create_empty_folder(&a, "notes").await;

        // Sync to B → B learns the folder node and materializes the empty directory.
        full_sync(&a, &b).await;
        assert!(
            empty_dir_exists(&fs_b, "notes").await,
            "B materializes the synced empty folder as a real empty directory"
        );
        assert_eq!(
            folder_nodes_at(&b, "notes"),
            1,
            "B has the alive folder node in its Index"
        );

        // A deletes the (still-empty) folder node; sync → B removes the empty directory.
        delete_folder(&a, id_a, "notes").await;

        full_sync(&a, &b).await;
        assert!(
            !fs_b.exists("notes").await.unwrap(),
            "B removes the now-tombstoned empty folder's directory"
        );
        assert_eq!(
            folder_nodes_at(&b, "notes"),
            0,
            "B's folder node is tombstoned (no alive folder at the path)"
        );
    }

    /// A freshly-loaded vault re-materializes its tracked empty folders from the Index on
    /// boot (INV-1.5a via reconcile), INCLUDING a nested folder's parent chain. An empty
    /// directory leaves no `.md` on disk, so a cold load that did not consult the folder
    /// set would silently drop it — boot reconcile's `materialize_folders` is what
    /// re-creates it. The folder node is created in the Index only (no `mkdir`), so the
    /// directory is genuinely absent until the reload's reconcile materializes it.
    #[tokio::test]
    async fn boot_reconcile_rematerializes_empty_folder() {
        let (vault, fs) = one_vault().await;
        // Stage a nested empty folder node in the Index. `create_folder` is catalog-only
        // (it does not touch disk), so neither `archive/` nor `archive/old/` exists on
        // disk yet — exactly the state a fresh clone has (the Index `.loro` synced, the
        // empty directories not).
        create_empty_folder(&vault, "archive/old").await;
        assert!(
            !fs.exists("archive/old").await.unwrap() && !fs.exists("archive").await.unwrap(),
            "precondition: the empty directories are absent before reload (create_folder is catalog-only)"
        );

        // Reload from the persisted Index — boot reconcile runs `materialize_folders`.
        let reloaded = reload(vault, &fs).await;

        // Boot reconcile re-materialized the tracked empty folder AND its parent chain.
        assert!(
            empty_dir_exists(&fs, "archive/old").await,
            "reload re-materializes the nested tracked empty folder from the Index"
        );
        assert!(
            fs.exists("archive").await.unwrap(),
            "the parent folder is materialized too (the chain)"
        );
        assert_eq!(
            folder_nodes_at(&reloaded, "archive/old"),
            1,
            "the reloaded Index still holds the empty folder node"
        );
    }

    /// Materialize NEVER recursively deletes a non-empty directory (INV-3): a folder node
    /// is tombstoned, but its on-disk directory still holds an UNTRACKED file (something
    /// the user dropped there, or a concurrent peer's content) — the directory must be
    /// LEFT IN PLACE, never `rm -rf`'d. Driven through a real inbound apply so the apply
    /// tail's `materialize_folders` runs against the tombstoned-folder + non-empty-dir state.
    #[tokio::test]
    async fn materialize_never_recursively_deletes_nonempty_dir() {
        let (a, b, _fs_a, fs_b) = two_vaults().await;

        // A creates an empty `proj/` folder; both converge with the directory on B.
        let id_a = create_empty_folder(&a, "proj").await;
        full_sync(&a, &b).await;
        assert!(
            empty_dir_exists(&fs_b, "proj").await,
            "B has the empty proj/ dir"
        );

        // An UNTRACKED file appears in B's `proj/` directory (not indexed — e.g. dropped
        // by the user, or a sidecar the library doesn't manage). It is NOT a vault `.md`
        // node; `materialize_folders` must treat the directory as non-empty.
        fs_b.write("proj/untracked.txt", b"not a tracked note")
            .await
            .unwrap();

        // A deletes the `proj/` folder node; sync delivers the tombstone to B, whose
        // apply-tail materialize runs against "tombstoned folder + non-empty dir".
        delete_folder(&a, id_a, "proj").await;
        full_sync(&a, &b).await;

        // The directory and its untracked file survive — no recursive delete (INV-3).
        assert!(
            fs_b.exists("proj").await.unwrap(),
            "the directory is preserved because it is non-empty (INV-3 — no recursive delete)"
        );
        assert_eq!(
            fs_b.read("proj/untracked.txt").await.unwrap(),
            b"not a tracked note",
            "the untracked file inside the tombstoned folder is untouched"
        );
    }
}
