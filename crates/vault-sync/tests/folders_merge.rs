//! Folder-structural acceptance tests — folder-collision merge (AC-OQ5) and the
//! ≥3-replica cross-replica determinism of the folder rules (INV-5.3c).
//!
//! Two replicas independently create distinct FOLDER nodes at one display path (each
//! inserts its own `proj/` folder node when it indexes a file under `proj/`). On sync
//! the two folder nodes collide; the resolver merges them into the min-TreeID survivor
//! and unions their children — and any same-name file the union surfaces falls to the
//! file cascade. These tests pin that merge in both sync directions and across three
//! replicas pumped to quiescence in arbitrary order.
//!
//! Everything runs against `InMemoryFs` — no test touches a real vault. The
//! replica/handshake/edit helpers live in the shared [`common`] harness.

mod common;
use common::*;

use vault_sync::FileSystem;

use std::collections::BTreeSet;
use uuid::Uuid;

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

            let notes_conflict = conflict_path_for("proj/Notes.md", &notes_loser);
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
            expected.insert(conflict_path_for("proj/Shared.md", loser));
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
