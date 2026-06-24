//! Folder-structural acceptance tests — the folder-safe move primitive
//! `Index::move_subtree` (INV-1.5b) and its clean-mover edge cases.
//!
//! A folder move is ONE `tree.mov` on the folder node; loro carries the
//! descendants structurally for free, so the document content never moves and
//! every descendant keeps its UUID. The primitive's job is to keep the
//! denormalized per-file `path` meta + the path↔node caches correct after that
//! single re-parent — the per-file `move_node` does not (it touches only the one
//! node it moves). These tests pin: descendant UUID preservation + zero content
//! re-transfer across a sync, the descendant `path`-meta rewrite (load-bearing for
//! the deleted-paths guard), the folder-safe-vs-raw-`move_node` cache contrast, and
//! that the primitive stays policy-free (refuses an occupied target, errors on a
//! missing source, no-ops on an identical move).
//!
//! Everything runs against `InMemoryFs` — no test touches a real vault. The
//! replica/handshake/edit helpers live in the shared [`common`] harness.

mod common;
use common::*;

use vault_sync::{FileSystem, IndexError, content_doc_path};

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

    /// S2 (fail-loud): `delete_folder` on a folder that still has an ALIVE child is
    /// REFUSED with an `Err`, never silently tombstoned. The whole-folder-delete contract
    /// is to per-file-`delete_node` the contents FIRST (HONORING each) and only then
    /// `delete_folder` the empty node; tombstoning a folder with a live file child would
    /// sweep that child into the RESCUE class (its `parent` stays `Node(folder)`), so the
    /// reactive rescue would resurrect a file the user meant to delete. The guard turns
    /// that future-daemon bug (skipping the per-file deletes) into a loud failure instead
    /// of silent data loss — and the folder stays alive (the refused delete is a no-op).
    #[tokio::test]
    async fn delete_folder_refuses_a_folder_with_a_live_child() {
        let (a, fs_a) = one_vault().await;
        // `proj/` holds a live file — the daemon has NOT per-file-deleted it.
        write_and_index(&a, &fs_a, "proj/keep.md", "# keep\n\nstill here").await;

        let err = a.index().delete_folder("proj").unwrap_err();
        assert!(
            matches!(err, IndexError::TreeOperation(_)),
            "deleting a folder with a live child fails loud (S2), got {err:?}"
        );

        // The folder and its live child are untouched — the refused delete is a no-op.
        assert_eq!(
            folder_nodes_at(&a, "proj"),
            1,
            "the folder stays alive after the refused delete"
        );
        assert!(
            a.index().node_for_path("proj/keep.md").is_some(),
            "the live child is untouched by the refused delete"
        );

        // After the contract is honored — per-file-delete the child first — `delete_folder`
        // succeeds and tombstones the now-empty folder.
        a.index().delete_node("proj/keep.md").unwrap();
        a.index().rebuild_caches();
        assert!(
            a.index().delete_folder("proj").unwrap(),
            "with the contents per-file-deleted first, delete_folder tombstones the empty folder"
        );
    }
}
