//! Folder-structural acceptance tests — the empty-folder lifecycle (AC-INV-1.5a) and
//! whole-folder delete + reactive orphan rescue (AC-EC-7 / OQ-6).
//!
//! A first-class empty folder node syncs to peers and materializes as a real empty
//! DIRECTORY, and deleting it removes that directory — but only when the directory is
//! empty (INV-3 never recursively deletes). A whole-folder delete is driven as per-file
//! `delete_node` for each removed file THEN `delete_folder` on the now-empty node; if a
//! peer concurrently ADDED a file under that folder, the reactive rescue revives the
//! folder chain and re-homes the swept add, so deleting a folder never silently loses a
//! doc a peer concurrently put in it.
//!
//! Everything runs against `InMemoryFs` — no test touches a real vault. The
//! replica/handshake/edit helpers live in the shared [`common`] harness.

mod common;
use common::*;

use vault_sync::FileSystem;

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

// =================== AC-EC-7/OQ-6 — folder-delete + reactive orphan rescue ===================
//
// Model II (`Index::delete_folder`): a whole-folder delete is driven as per-file
// `delete_node` for each removed file (stamping each `parent == Deleted`, the HONOR
// marker) THEN `delete_folder` on the now-empty folder node. If a peer concurrently
// ADDED a file under that folder, the folder-node delete sweeps it (it inherits the
// tombstone) — its own `parent` stays `Node(folder)`, the RESCUE marker. The reactive
// rescue (`rescue_swept_orphans`, fired inside the apply path before delete-detection)
// revives the folder chain to live and re-homes the swept add to its original path, so
// EC-7 holds: deleting a folder never silently loses a doc a peer concurrently put in it.
//
// Every test drives the REAL `process_message` apply path (via the handshake helpers),
// checks BOTH replicas, and runs BOTH sync directions — the rescue is symmetric, no
// locality. "Byte-identical convergence" is asserted via `assert_converged`.

mod ac_ec7_folder_delete_rescue {
    use super::*;

    /// Remove a whole directory the Model-II way on `vault`+`fs`: per-file `delete_node`
    /// each `.md` directly under `dir` (the HONOR-stamping genuine deletes), drop each on
    /// disk, then `delete_folder(dir)` the now-empty folder node. Flushes the Index.
    /// Mirrors what the daemon's folder-delete detection would emit in P4.
    async fn delete_folder_with_files(vault: &V, fs: &Fs, dir: &str) {
        let prefix = format!("{dir}/");
        let children: Vec<String> = vault
            .list_files()
            .await
            .unwrap()
            .into_iter()
            // Only the files DIRECTLY in `dir` (a nested file is removed when its own
            // sub-folder is deleted; tests that need nested deletes call this per level).
            .filter(|p| p.starts_with(&prefix) && !p[prefix.len()..].contains('/'))
            .collect();
        for child in &children {
            vault.index().delete_node(child).unwrap();
            fs.delete(child).await.ok();
        }
        vault.index().delete_folder(dir).unwrap();
        vault.save_index().await.unwrap();
    }

    /// A clean whole-folder delete with NO concurrent add: the folder and its files go,
    /// the directory is removed on both replicas, NOTHING is rescued, and they converge.
    /// (The honor-only path — every dead node reads `parent == Deleted`.)
    #[tokio::test]
    async fn clean_folder_delete_no_rescue() {
        for forward in [true, false] {
            let (a, b, fs_a, fs_b) = two_vaults().await;
            write_and_index(&a, &fs_a, "proj/a.md", "# a\n\nbody a").await;
            write_and_index(&a, &fs_a, "proj/b.md", "# b\n\nbody b").await;
            sync_both_ways(&a, &b).await;

            // A removes the whole folder; no peer added anything.
            delete_folder_with_files(&a, &fs_a, "proj").await;

            if forward {
                sync_both_ways(&a, &b).await;
            } else {
                sync_both_ways(&b, &a).await;
            }

            let label = if forward { "A->B" } else { "B->A" };
            assert!(
                md_files(&b).await.is_empty(),
                "{label}: B's files are gone (whole folder deleted)"
            );
            assert!(md_files(&a).await.is_empty(), "{label}: A's files are gone");
            assert!(
                !fs_b.exists("proj").await.unwrap(),
                "{label}: B's now-empty proj/ directory is removed"
            );
            assert_eq!(
                folder_nodes_at(&b, "proj"),
                0,
                "{label}: no alive proj/ folder node on B"
            );
            assert_converged(&a, &fs_a, &b, &fs_b).await;
        }
    }

    /// THE HEADLINE (EC-7): A deletes `proj/`'s files + the folder while B concurrently
    /// adds `proj/new.md`. After convergence the concurrent add is RESCUED — `proj/`
    /// revived holding `proj/new.md` at its original path with its body intact (INV-3) —
    /// while A's genuinely-deleted files stay deleted. Byte-identical, both directions.
    #[tokio::test]
    async fn delete_vs_concurrent_add_rescues_orphan() {
        for forward in [true, false] {
            let (a, b, fs_a, fs_b) = two_vaults().await;
            write_and_index(&a, &fs_a, "proj/keep.md", "# keep\n\nkeep body").await;
            write_and_index(&a, &fs_a, "proj/gone.md", "# gone\n\ngone body").await;
            sync_both_ways(&a, &b).await;

            // CONCURRENT: A removes the whole folder; B adds a file inside it.
            delete_folder_with_files(&a, &fs_a, "proj").await;
            write_and_index(&b, &fs_b, "proj/new.md", "# new\n\nNEW BODY important").await;

            if forward {
                sync_both_ways(&a, &b).await;
            } else {
                sync_both_ways(&b, &a).await;
            }
            let label = if forward { "A->B" } else { "B->A" };

            // The concurrent add survives at its original path on BOTH replicas, body intact.
            for (v, fs, who) in [(&a, &fs_a, "A"), (&b, &fs_b, "B")] {
                assert!(
                    md_files(v).await.contains("proj/new.md"),
                    "{label}/{who}: the rescued concurrent add proj/new.md is present"
                );
                assert_eq!(
                    read_md_str(fs, "proj/new.md").await,
                    "# new\n\nNEW BODY important",
                    "{label}/{who}: rescued body is intact (INV-3)"
                );
                // A's genuinely-deleted files stay deleted.
                assert!(
                    !md_files(v).await.contains("proj/keep.md"),
                    "{label}/{who}: the explicitly-deleted keep.md stays deleted"
                );
                assert!(
                    !md_files(v).await.contains("proj/gone.md"),
                    "{label}/{who}: the explicitly-deleted gone.md stays deleted"
                );
                // proj/ is alive (it holds a live child) and is exactly one folder node.
                assert_eq!(
                    folder_nodes_at(v, "proj"),
                    1,
                    "{label}/{who}: exactly one surviving proj/ folder node (revives merged)"
                );
            }
            assert_converged(&a, &fs_a, &b, &fs_b).await;
        }
    }

    /// CRUX (PROBE C): the concurrent add is directly inside an EXPLICITLY-deleted
    /// SUB-folder. A deletes `proj/sub/`'s file + the `sub/` folder + the `proj/` folder
    /// while B adds `proj/sub/added.md`. The add was never itself deleted (only swept by
    /// `sub/`'s deletion) so it is rescued — the rule keys on "was THIS node explicitly
    /// deleted", not "is this node under a deleted folder".
    #[tokio::test]
    async fn concurrent_add_inside_deleted_subfolder_is_rescued() {
        for forward in [true, false] {
            let (a, b, fs_a, fs_b) = two_vaults().await;
            write_and_index(&a, &fs_a, "proj/sub/pre.md", "# pre\n\npre body").await;
            sync_both_ways(&a, &b).await;

            // A removes proj/sub/ then proj/ (each level the Model-II way), bottom-up.
            delete_folder_with_files(&a, &fs_a, "proj/sub").await;
            // proj/ now has no files (sub was its only child) — delete the folder node.
            a.index().delete_folder("proj").unwrap();
            a.save_index().await.unwrap();

            // B concurrently adds a file under the sub-folder A is deleting.
            write_and_index(&b, &fs_b, "proj/sub/added.md", "# added\n\nADDED body").await;

            if forward {
                sync_both_ways(&a, &b).await;
            } else {
                sync_both_ways(&b, &a).await;
            }
            let label = if forward { "A->B" } else { "B->A" };

            for (v, fs, who) in [(&a, &fs_a, "A"), (&b, &fs_b, "B")] {
                assert!(
                    md_files(v).await.contains("proj/sub/added.md"),
                    "{label}/{who}: the add inside the deleted sub-folder is rescued"
                );
                assert_eq!(
                    read_md_str(fs, "proj/sub/added.md").await,
                    "# added\n\nADDED body",
                    "{label}/{who}: rescued sub-folder add body intact"
                );
                assert!(
                    !md_files(v).await.contains("proj/sub/pre.md"),
                    "{label}/{who}: the explicitly-deleted pre.md stays deleted"
                );
                assert_eq!(
                    folder_nodes_at(v, "proj/sub"),
                    1,
                    "{label}/{who}: the proj/sub chain is revived to one folder node"
                );
            }
            assert_converged(&a, &fs_a, &b, &fs_b).await;
        }
    }

    /// NESTED DEPTH: a swept GRANDCHILD. A deletes `a/b/orig.md` + the `a/b/` + `a/`
    /// folders while B adds `a/b/deep.md`. The grandchild is rescued and the multi-level
    /// `a/b/` chain is revived to live.
    #[tokio::test]
    async fn nested_swept_grandchild_is_rescued() {
        for forward in [true, false] {
            let (a, b, fs_a, fs_b) = two_vaults().await;
            write_and_index(&a, &fs_a, "a/b/orig.md", "# orig\n\norig body").await;
            sync_both_ways(&a, &b).await;

            // A removes a/b/ then a/ (a/b/ was a/'s only child).
            delete_folder_with_files(&a, &fs_a, "a/b").await;
            a.index().delete_folder("a").unwrap();
            a.save_index().await.unwrap();

            // B concurrently adds a grandchild two levels deep.
            write_and_index(&b, &fs_b, "a/b/deep.md", "# deep\n\nDEEP body").await;

            if forward {
                sync_both_ways(&a, &b).await;
            } else {
                sync_both_ways(&b, &a).await;
            }
            let label = if forward { "A->B" } else { "B->A" };

            for (v, fs, who) in [(&a, &fs_a, "A"), (&b, &fs_b, "B")] {
                assert!(
                    md_files(v).await.contains("a/b/deep.md"),
                    "{label}/{who}: the swept grandchild is rescued at its original depth"
                );
                assert_eq!(
                    read_md_str(fs, "a/b/deep.md").await,
                    "# deep\n\nDEEP body",
                    "{label}/{who}: rescued grandchild body intact"
                );
                assert!(
                    !md_files(v).await.contains("a/b/orig.md"),
                    "{label}/{who}: the explicitly-deleted orig.md stays deleted"
                );
                assert_eq!(
                    folder_nodes_at(v, "a"),
                    1,
                    "{label}/{who}: the a/ ancestor is revived"
                );
                assert_eq!(
                    folder_nodes_at(v, "a/b"),
                    1,
                    "{label}/{who}: the a/b/ chain is revived to live"
                );
            }
            assert_converged(&a, &fs_a, &b, &fs_b).await;
        }
    }

    /// HONOR (no false rescue): an explicitly `delete_node`d file — NOT swept by a folder
    /// delete — must NOT be rescued. A deletes a single file `notes/old.md` (its parent
    /// folder stays alive, other files remain) while B edits a sibling; the deleted file
    /// reads `parent == Deleted`, so the rescue leaves it deleted (it was the user's
    /// explicit deletion, not a swept concurrent add).
    #[tokio::test]
    async fn explicit_file_delete_is_not_rescued() {
        for forward in [true, false] {
            let (a, b, fs_a, fs_b) = two_vaults().await;
            write_and_index(&a, &fs_a, "notes/old.md", "# old\n\nold body").await;
            write_and_index(&a, &fs_a, "notes/stay.md", "# stay\n\nstay body").await;
            sync_both_ways(&a, &b).await;

            // A explicitly deletes ONE file (the folder + the sibling stay alive).
            a.index().delete_node("notes/old.md").unwrap();
            fs_a.delete("notes/old.md").await.ok();
            a.save_index().await.unwrap();
            // B concurrently edits the sibling, so there is genuine traffic to merge.
            write_and_index(&b, &fs_b, "notes/stay.md", "# stay\n\nstay body EDITED").await;

            if forward {
                sync_both_ways(&a, &b).await;
            } else {
                sync_both_ways(&b, &a).await;
            }
            let label = if forward { "A->B" } else { "B->A" };

            for (v, who) in [(&a, "A"), (&b, "B")] {
                assert!(
                    !md_files(v).await.contains("notes/old.md"),
                    "{label}/{who}: the explicitly-deleted file is NOT resurrected by the rescue"
                );
                assert!(
                    md_files(v).await.contains("notes/stay.md"),
                    "{label}/{who}: the sibling survives"
                );
            }
            assert_converged(&a, &fs_a, &b, &fs_b).await;
        }
    }

    /// RESCUE TARGET OCCUPIED: a swept orphan whose original path is concurrently taken
    /// by a DISTINCT live file. A deletes `proj/`'s file + folder AND, before the delete,
    /// B added `proj/new.md`; meanwhile a THIRD distinct document is also placed at
    /// `proj/new.md` (a different UUID) so the rescue's re-home target is occupied. The
    /// rescue re-homes the orphan and the resulting same-path collision falls to the
    /// existing file cascade (INV-5) — no document is lost, deterministic both ways.
    #[tokio::test]
    async fn rescue_target_occupied_falls_to_cascade() {
        for forward in [true, false] {
            let (a, b, fs_a, fs_b) = two_vaults().await;
            write_and_index(&a, &fs_a, "proj/seed.md", "# seed\n\nseed body").await;
            sync_both_ways(&a, &b).await;

            // A removes the whole proj/ folder.
            delete_folder_with_files(&a, &fs_a, "proj").await;

            // B (concurrently) adds proj/new.md AND, also on B, a DISTINCT second document
            // that will end up colliding at the same path post-rescue. To get two distinct
            // UUIDs at one path we create new.md on B and a same-path doc on A's side too:
            // A re-creates proj/new.md as its OWN distinct doc after the folder delete.
            write_and_index(&b, &fs_b, "proj/new.md", "# new B\n\nB body").await;
            write_and_index(&a, &fs_a, "proj/new.md", "# new A\n\nA body").await;

            let uuid_a = uuid_at(&a, "proj/new.md");
            let uuid_b = uuid_at(&b, "proj/new.md");
            assert_ne!(
                uuid_a, uuid_b,
                "the two proj/new.md docs are distinct documents"
            );

            if forward {
                sync_both_ways(&a, &b).await;
            } else {
                sync_both_ways(&b, &a).await;
            }
            let label = if forward { "A->B" } else { "B->A" };

            // NEITHER document is lost: the survivor keeps proj/new.md, the loser is at the
            // full-UUID conflict path. Both replicas agree (the cascade is deterministic).
            let loser = uuid_a.max(uuid_b);
            let conflict = conflict_path_for("proj/new.md", &loser);
            for (v, who) in [(&a, "A"), (&b, "B")] {
                let files = md_files(v).await;
                assert!(
                    files.contains("proj/new.md"),
                    "{label}/{who}: a survivor keeps proj/new.md"
                );
                assert!(
                    files.contains(&conflict),
                    "{label}/{who}: the loser is preserved at the conflict path {conflict} (no loss)"
                );
            }
            assert_converged(&a, &fs_a, &b, &fs_b).await;
        }
    }

    /// MOVE-THEN-SWEPT (S4): the swept concurrent change is a MOVE, not a fresh add. B
    /// moves an existing file `loose.md` INTO `proj/` (→ `proj/loose.md`) while A deletes
    /// the whole `proj/` folder. After convergence the moved file is RESCUED at its NEW
    /// path `proj/loose.md` — the rescue reads `original_path` from the node's
    /// `TREE_META_PATH`, which the move rewrote to the destination, so a moved-then-swept
    /// orphan re-homes to where it was moved TO (not its pre-move path). Body intact, both
    /// replicas, both directions.
    #[tokio::test]
    async fn moved_into_deleted_folder_is_rescued_at_new_path() {
        for forward in [true, false] {
            let (a, b, fs_a, fs_b) = two_vaults().await;
            // `proj/` exists (via an anchor file A will delete) and `loose.md` lives at root.
            write_and_index(&a, &fs_a, "proj/anchor.md", "# anchor\n\nanchor body").await;
            write_and_index(&a, &fs_a, "loose.md", "# loose\n\nLOOSE body").await;
            sync_both_ways(&a, &b).await;

            let loose_uuid = uuid_at(&a, "loose.md");

            // CONCURRENT: A removes the whole proj/ folder; B moves loose.md INTO proj/.
            delete_folder_with_files(&a, &fs_a, "proj").await;
            move_file(&b, &fs_b, "loose.md", "proj/loose.md").await;

            if forward {
                sync_both_ways(&a, &b).await;
            } else {
                sync_both_ways(&b, &a).await;
            }
            let label = if forward { "A->B" } else { "B->A" };

            for (v, fs, who) in [(&a, &fs_a, "A"), (&b, &fs_b, "B")] {
                // The moved file is rescued at its NEW path (where B moved it TO), under its
                // stable UUID, with its body intact.
                assert!(
                    md_files(v).await.contains("proj/loose.md"),
                    "{label}/{who}: the moved-then-swept file is rescued at its new path"
                );
                assert_eq!(
                    uuid_at(v, "proj/loose.md"),
                    loose_uuid,
                    "{label}/{who}: the rescued move keeps its stable UUID"
                );
                assert_eq!(
                    read_md_str(fs, "proj/loose.md").await,
                    "# loose\n\nLOOSE body",
                    "{label}/{who}: rescued moved body is intact (INV-3)"
                );
                // The old root path is vacated (the file moved away), and A's genuinely
                // deleted anchor stays deleted.
                assert!(
                    !md_files(v).await.contains("loose.md"),
                    "{label}/{who}: the pre-move root path is vacated"
                );
                assert!(
                    !md_files(v).await.contains("proj/anchor.md"),
                    "{label}/{who}: the explicitly-deleted anchor stays deleted"
                );
                assert_eq!(
                    folder_nodes_at(v, "proj"),
                    1,
                    "{label}/{who}: proj/ is revived to exactly one folder node"
                );
            }
            assert_converged(&a, &fs_a, &b, &fs_b).await;
        }
    }

    /// DETERMINISM under reordering: with THREE replicas, a folder delete vs a concurrent
    /// add converges byte-identically regardless of the pump order — the rescue's
    /// classification + placement is a pure function of merged state, so every replica
    /// computes the same partition. Driven via `pump_to_quiescence` (every ordered pair).
    #[tokio::test]
    async fn rescue_is_deterministic_across_three_replicas() {
        let (a, b, c, fs_a, fs_b, fs_c) = three_vaults().await;
        write_and_index(&a, &fs_a, "proj/base.md", "# base\n\nbase body").await;
        pump_to_quiescence(&[&a, &b, &c]).await;

        // A removes the whole folder; C concurrently adds a file inside it.
        delete_folder_with_files(&a, &fs_a, "proj").await;
        write_and_index(&c, &fs_c, "proj/added.md", "# added\n\nADDED body").await;

        pump_to_quiescence(&[&a, &b, &c]).await;

        // All three converge: the rescued add present + intact, base gone, one proj/ node.
        for (v, fs, who) in [(&a, &fs_a, "A"), (&b, &fs_b, "B"), (&c, &fs_c, "C")] {
            assert!(
                md_files(v).await.contains("proj/added.md"),
                "{who}: the rescued add is present after quiescence"
            );
            assert_eq!(
                read_md_str(fs, "proj/added.md").await,
                "# added\n\nADDED body",
                "{who}: rescued body intact"
            );
            assert!(
                !md_files(v).await.contains("proj/base.md"),
                "{who}: the deleted base.md stays deleted"
            );
            assert_eq!(
                folder_nodes_at(v, "proj"),
                1,
                "{who}: one surviving proj/ node"
            );
        }
        // Byte-identical across all pairs.
        assert_converged(&a, &fs_a, &b, &fs_b).await;
        assert_converged(&b, &fs_b, &c, &fs_c).await;
    }
}
