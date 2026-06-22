//! The Phase-2 capstone: the unified structural-resolution pass composing ALL THREE
//! rule families at interacting paths, and the cross-replica determinism battery
//! (INV-5.3 / INV-4 — the convergence PROOF for the whole P2 surface).
//!
//! `resolve_structure` is ONE fixpoint pass with three rule families that FEED each
//! other (each mutates paths, so one's output is another's input, INV-5.3):
//!
//!   1. **folder-merge** — collapse ≥2 same-path folder nodes to the min-TreeID
//!      survivor, union their alive children.
//!   2. **file-vs-folder** — at a path holding both a file node and a folder node,
//!      the folder wins; the file relocates INSIDE it at `<folder>/<filename>`.
//!   3. **file cascade** — at a path with ≥2 distinct-UUID files, min-UUID survives;
//!      every other loser is renamed to `(conflict <full-uuid>)`.
//!
//! Each family alone (plus the two pairwise compositions — folder-merge→collision and
//! file-vs-folder→relocate→collision) is pinned by the P2a–P2e suites in `conflict.rs`
//! / `folders.rs`. This file proves the WHOLE composition: a single merged state where
//! the folder-merge union itself SURFACES both a file-vs-folder collision and a
//! relocate-onto-occupied file collision, plus an independent file collision — all
//! resolved in one pass — converges to byte-identical state on every replica regardless
//! of the order it applied the inbound CRDT ops (the AC-INV-5-BATCH analogue for the
//! structural pass, AC-STRUCT-COMPOSE), and an N-replica mixed-collision mesh converges
//! identically under any sync order.
//!
//! **What "different op orders" means here.** The seam ships an opaque loro Index
//! snapshot that imports as one atomic CRDT merge — there is no per-op knob at the
//! seam. The order a replica actually *integrates* the inbound CRDT ops IS determined by
//! the order it exchanges and merges messages with its peers; so the order-independence
//! lever is the sync direction / pump order. Every capstone scenario is therefore
//! delivered under BOTH a fixed and a shuffled sync order and asserted byte-identical —
//! a resolution that secretly depended on integration order (a locality bug,
//! INV-5.3(c)) would diverge between the two.
//!
//! Everything runs against `InMemoryFs` — no test touches a real on-disk vault, and
//! there is no iroh/transport (the seam is bytes, pumped by the [`common`] harness).

mod common;
use common::*;

use std::collections::BTreeSet;
use uuid::Uuid;
use vault_sync::FileSystem;

// ============================ AC-STRUCT-COMPOSE — the capstone ============================
//
// Construct ONE merged state where folder-merge, file-vs-folder, AND the file cascade
// apply at INTERACTING paths — the file-vs-folder and the relocate-onto-occupied
// collision are both SURFACED by the folder-merge union, so the three families genuinely
// feed each other rather than merely coexisting. Deliver it to two replicas under
// different integration orders (fixed vs shuffled sync) and assert byte-identical state.

mod ac_struct_compose {
    use super::*;

    /// The interacting-path construction, built on two fresh replicas. Returns the
    /// replicas + filesystems and the UUIDs needed to name the expected survivors and
    /// conflict paths.
    ///
    /// Under one `proj/` folder the two replicas independently create:
    ///   - A: a folder `proj/foo.md/` (via `proj/foo.md/inside.md`) that ALSO already
    ///     holds a live `proj/foo.md/foo.md` — the eventual relocation target — plus a
    ///     distinct `proj/a.md` and a colliding `proj/Notes.md`.
    ///   - B: a FILE `proj/foo.md`, a distinct `proj/b.md`, and a colliding
    ///     `proj/Notes.md` (different content).
    ///
    /// Each replica's `proj/` is a distinct folder node, so on merge:
    ///   (1) **folder-merge** unions the two `proj/` nodes (and the two `proj/foo.md/`
    ///       observations — A owns the folder, B's same-named FILE is a different node),
    ///   (2) the union surfaces **file-vs-folder** at `proj/foo.md` (A's folder vs B's
    ///       file) → B's file relocates to `proj/foo.md/foo.md`,
    ///   (3) that relocation target is OCCUPIED (A's live `proj/foo.md/foo.md`) → the
    ///       **file cascade** resolves the two there (min-UUID wins, loser → conflict),
    ///   (4) and the independent `proj/Notes.md` collision resolves by the cascade too.
    struct Compose {
        a: V,
        b: V,
        fs_a: Fs,
        fs_b: Fs,
        /// `proj/Notes.md` colliders (A's and B's).
        notes_a: Uuid,
        notes_b: Uuid,
        /// The live occupant at the relocation target `proj/foo.md/foo.md` (A's).
        target_occupant: Uuid,
        /// B's file `proj/foo.md` that relocates onto the occupied target.
        relocating_file: Uuid,
    }

    async fn build() -> Compose {
        let (a, b, fs_a, fs_b) = two_vaults().await;

        // A's `proj/`: a folder `proj/foo.md/` holding the relocation target +
        // a nested file, a distinct file, and a colliding Notes.md.
        write_and_index(
            &a,
            &fs_a,
            "proj/foo.md/foo.md",
            "# Occupant\n\nA occupant.\n",
        )
        .await;
        write_and_index(
            &a,
            &fs_a,
            "proj/foo.md/inside.md",
            "# Inside\n\nA inside.\n",
        )
        .await;
        write_and_index(&a, &fs_a, "proj/a.md", "# A\n\nAlpha.\n").await;
        write_and_index(&a, &fs_a, "proj/Notes.md", "# Notes A\n\nFrom A.\n").await;

        // B's `proj/`: a FILE at `proj/foo.md` (collides with A's folder), a distinct
        // file, and a colliding Notes.md with DIFFERENT content.
        write_and_index(&b, &fs_b, "proj/foo.md", "# Foo File\n\nB foo file.\n").await;
        write_and_index(&b, &fs_b, "proj/b.md", "# B\n\nBeta.\n").await;
        write_and_index(&b, &fs_b, "proj/Notes.md", "# Notes B\n\nFrom B.\n").await;

        Compose {
            notes_a: uuid_at(&a, "proj/Notes.md"),
            notes_b: uuid_at(&b, "proj/Notes.md"),
            target_occupant: uuid_at(&a, "proj/foo.md/foo.md"),
            relocating_file: uuid_at(&b, "proj/foo.md"),
            a,
            b,
            fs_a,
            fs_b,
        }
    }

    /// Assert the converged state of a built `Compose` matches the deterministic
    /// composition outcome on the given replica — same on every replica.
    ///
    /// The folder-merge survivor is a min-TreeID choice with no observable content
    /// effect (folders carry no content), so it is checked structurally (exactly one
    /// `proj/foo.md` folder node, the union present), not by a predicted TreeID.
    async fn assert_composed(c: &Compose, label: &str, v: &V, fs: &Fs) {
        // (1) folder-merge: exactly ONE surviving `proj/` and ONE `proj/foo.md/` folder
        // node — the two same-path folder observations merged, no stray duplicate.
        assert_eq!(
            folder_nodes_at(v, "proj"),
            1,
            "[{label}] one surviving proj/ folder"
        );
        assert_eq!(
            folder_nodes_at(v, "proj/foo.md"),
            1,
            "[{label}] file-vs-folder kept the folder: one surviving proj/foo.md/ folder node"
        );

        // (2)+(3) file-vs-folder → relocate-onto-occupied → cascade at proj/foo.md/foo.md:
        // the folder wins proj/foo.md (B's file relocates INSIDE), and at the occupied
        // target the min-UUID of {A's occupant, B's relocated file} wins, the other gets
        // a conflict file off the target.
        let target_winner = c.target_occupant.min(c.relocating_file);
        let target_loser = c.target_occupant.max(c.relocating_file);
        let target_conflict = conflict_path_for("proj/foo.md/foo.md", &target_loser);
        assert_eq!(
            uuid_at(v, "proj/foo.md/foo.md"),
            target_winner,
            "[{label}] min-UUID wins the occupied relocation target"
        );
        assert_eq!(
            uuid_at(v, &target_conflict),
            target_loser,
            "[{label}] the relocated loser lands at a full-UUID conflict file off the target"
        );

        // (4) the independent proj/Notes.md collision: min-UUID survives, loser → conflict.
        let notes_winner = c.notes_a.min(c.notes_b);
        let notes_loser = c.notes_a.max(c.notes_b);
        let notes_conflict = conflict_path_for("proj/Notes.md", &notes_loser);
        assert_eq!(
            uuid_at(v, "proj/Notes.md"),
            notes_winner,
            "[{label}] min-UUID wins proj/Notes.md"
        );
        assert_eq!(
            uuid_at(v, &notes_conflict),
            notes_loser,
            "[{label}] the Notes.md loser lands at its full-UUID conflict file"
        );

        // The EXACT union of `.md` files — the union members plus every resolved
        // survivor/conflict, and NOTHING else. A stray intermediate-named conflict file
        // (from a per-op rather than once-on-merged-state pass) or a lost document would
        // fail this exact-set assertion — the no-instrument proof the pass fired once on
        // the fully-merged state (AC-STRUCT-COMPOSE, the AC-INV-5-BATCH analogue).
        let expected = BTreeSet::from([
            "proj/a.md".to_string(),
            "proj/b.md".to_string(),
            "proj/foo.md/inside.md".to_string(),
            "proj/foo.md/foo.md".to_string(),
            target_conflict.clone(),
            "proj/Notes.md".to_string(),
            notes_conflict.clone(),
        ]);
        assert_eq!(
            alive_md_paths(v).await,
            expected,
            "[{label}] exact union + resolved survivors/conflicts, no stray, nothing lost"
        );

        // INV-3 — every distinct document's content survives somewhere (the two Notes
        // bodies, both foo.md/foo.md bodies, B's relocated foo-file body): no silent loss.
        let all_bodies = {
            let mut s = String::new();
            for path in &expected {
                s.push_str(&read_md_str(fs, path).await);
                s.push('\n');
            }
            s
        };
        for needle in ["From A.", "From B.", "A occupant.", "B foo file."] {
            assert!(
                all_bodies.contains(needle),
                "[{label}] content '{needle}' survived the composition (INV-3)"
            );
        }
    }

    /// The capstone under a FIXED sync order: deliver the merged delta via a plain
    /// `sync_both_ways` (A initiates) and assert both replicas reach the identical,
    /// fully-composed state.
    #[tokio::test]
    async fn all_three_families_compose_under_fixed_order() {
        let c = build().await;
        sync_both_ways(&c.a, &c.b).await;

        assert_composed(&c, "A", &c.a, &c.fs_a).await;
        assert_composed(&c, "B", &c.b, &c.fs_b).await;
        assert_converged(&c.a, &c.fs_a, &c.b, &c.fs_b).await;
    }

    /// The SAME capstone under the REVERSED sync direction (B initiates): the
    /// composition is a pure function of merged state, so the reversed integration order
    /// must land on the byte-identical result — a direction-dependent (locality) bug
    /// would diverge here.
    #[tokio::test]
    async fn all_three_families_compose_under_reversed_order() {
        let c = build().await;
        sync_both_ways(&c.b, &c.a).await;

        assert_composed(&c, "A", &c.a, &c.fs_a).await;
        assert_composed(&c, "B", &c.b, &c.fs_b).await;
        assert_converged(&c.a, &c.fs_a, &c.b, &c.fs_b).await;
    }

    /// The capstone under a SHUFFLED pump order: drive convergence with the
    /// shuffled-pair pump (a seeded, reproducible visit order distinct from the fixed
    /// `sync_both_ways`) and assert the SAME fully-composed byte-identical state. Fixed
    /// and shuffled integration orders reaching the same state is the order-independence
    /// proof for the whole composition (INV-5.3(c) / INV-4).
    #[tokio::test]
    async fn all_three_families_compose_under_shuffled_order() {
        let c = build().await;
        // Several seeds → several distinct integration orders, all converging identically.
        for seed in [1u64, 7, 42, 1234] {
            pump_to_quiescence_shuffled(&[&c.a, &c.b], seed).await;
            assert_composed(&c, &format!("A (seed {seed})"), &c.a, &c.fs_a).await;
            assert_composed(&c, &format!("B (seed {seed})"), &c.b, &c.fs_b).await;
            assert_converged(&c.a, &c.fs_a, &c.b, &c.fs_b).await;
        }
    }
}

// ============================ cross-replica determinism battery ============================
//
// An N-replica mesh where every replica makes a MIX of divergent same-path creates,
// folder collisions, file-vs-folder, and folder-deletes-with-concurrent-adds (exercising
// the reactive rescue too) converges to byte-identical materialized state + identical
// Index VVs under ANY sync order — the INV-4/INV-5 capstone for the whole P2 surface.

mod cross_replica_determinism {
    use super::*;

    /// Remove a whole directory the Model-II way (per-file `delete_node` each `.md`
    /// directly under `dir`, drop each on disk, then `delete_folder` the now-empty
    /// folder node) — what the daemon's folder-delete detection emits in P4. The same
    /// primitive the EC-7 suite drives; duplicated minimally here so this battery is
    /// self-contained across test binaries.
    async fn delete_folder_with_files(vault: &V, fs: &Fs, dir: &str) {
        let prefix = format!("{dir}/");
        let children: Vec<String> = vault
            .list_files()
            .await
            .unwrap()
            .into_iter()
            .filter(|p| p.starts_with(&prefix) && !p[prefix.len()..].contains('/'))
            .collect();
        for child in &children {
            vault.index().delete_node(child).unwrap();
            fs.delete(child).await.ok();
        }
        vault.index().delete_folder(dir).unwrap();
        vault.save_index().await.unwrap();
    }

    /// Stage the SAME divergent four-collision mesh on a fresh set of `n` replicas (the
    /// i-th paired with its filesystem), so the fixed-order and shuffled-order runs start
    /// from byte-identical starting divergence. Returns the replicas and the UUIDs needed
    /// to name the expected survivors/conflicts.
    ///
    /// Across 4 replicas (A/B/C/D):
    ///   - **same-path file collision**: A, B, C each create a distinct `Root.md`.
    ///   - **folder collision**: A and B each create a distinct `team/` folder with a
    ///     same-named `team/Plan.md` (different content) + a distinct file each.
    ///   - **file-vs-folder**: C creates a FILE `team/Plan.md/`-shaped... no — C creates
    ///     a folder `docs.md/` (via `docs.md/x.md`), D creates a FILE `docs.md` → on
    ///     merge the folder wins and D's file relocates to `docs.md/docs.md`.
    ///   - **folder-delete + concurrent add (rescue)**: A creates `old/gone.md`, syncs
    ///     so every replica has it, then A deletes the whole `old/` folder while D
    ///     concurrently adds `old/added.md` → the add is rescued.
    struct Mesh {
        replicas: Vec<(V, Fs)>,
        root_uuids: Vec<Uuid>,
        plan_a: Uuid,
        plan_b: Uuid,
        docs_folder_child_uuid: Uuid,
        docs_file: Uuid,
    }

    async fn stage() -> Mesh {
        let replicas = n_vaults(4).await;
        let (a, fs_a) = (&replicas[0].0, &replicas[0].1);
        let (b, fs_b) = (&replicas[1].0, &replicas[1].1);
        let (c, fs_c) = (&replicas[2].0, &replicas[2].1);
        let (d, fs_d) = (&replicas[3].0, &replicas[3].1);

        // --- folder-delete + concurrent-add seed: every replica must first SHARE `old/`
        // so A's later delete is genuinely concurrent with D's add. Seed + converge it
        // before staging the rest of the divergence.
        write_and_index(a, fs_a, "old/gone.md", "# gone\n\ngone body").await;
        let refs: Vec<&V> = replicas.iter().map(|(v, _)| v).collect();
        pump_to_quiescence(&refs).await;

        // --- same-path file collision: A/B/C each create a distinct Root.md.
        write_and_index(a, fs_a, "Root.md", "# Root A\n\nfrom A").await;
        write_and_index(b, fs_b, "Root.md", "# Root B\n\nfrom B").await;
        write_and_index(c, fs_c, "Root.md", "# Root C\n\nfrom C").await;
        let root_uuids = vec![
            uuid_at(a, "Root.md"),
            uuid_at(b, "Root.md"),
            uuid_at(c, "Root.md"),
        ];

        // --- folder collision: A and B each build a distinct team/ with a same-named
        // team/Plan.md (different content) plus a distinct file each.
        write_and_index(a, fs_a, "team/Plan.md", "# Plan A\n\nplan from A").await;
        write_and_index(a, fs_a, "team/a.md", "# tA\n\nteam A file").await;
        write_and_index(b, fs_b, "team/Plan.md", "# Plan B\n\nplan from B").await;
        write_and_index(b, fs_b, "team/b.md", "# tB\n\nteam B file").await;
        let plan_a = uuid_at(a, "team/Plan.md");
        let plan_b = uuid_at(b, "team/Plan.md");

        // --- file-vs-folder: C builds a folder docs.md/ (via a child), D creates a FILE
        // docs.md → on merge the folder wins, D's file relocates to docs.md/docs.md.
        write_and_index(c, fs_c, "docs.md/x.md", "# inside docs\n\ndocs child").await;
        write_and_index(d, fs_d, "docs.md", "# docs file\n\nD docs file").await;
        let docs_folder_child_uuid = uuid_at(c, "docs.md/x.md");
        let docs_file = uuid_at(d, "docs.md");

        // --- folder-delete + concurrent add: A removes the whole old/ folder while D
        // concurrently adds old/added.md (the rescue case).
        delete_folder_with_files(a, fs_a, "old").await;
        write_and_index(d, fs_d, "old/added.md", "# added\n\nADDED body important").await;

        Mesh {
            replicas,
            root_uuids,
            plan_a,
            plan_b,
            docs_folder_child_uuid,
            docs_file,
        }
    }

    /// Assert the converged state of a staged `Mesh` on one replica matches the
    /// deterministic resolution of all four collisions — identical on every replica.
    async fn assert_mesh_resolved(m: &Mesh, label: &str, v: &V, fs: &Fs) {
        // same-path file collision at Root.md: global min-UUID wins, the other two →
        // their own full-UUID conflict files.
        let root_winner = *m.root_uuids.iter().min().unwrap();
        let root_conflicts: Vec<String> = m
            .root_uuids
            .iter()
            .filter(|u| **u != root_winner)
            .map(|u| conflict_path_for("Root.md", u))
            .collect();
        assert_eq!(
            uuid_at(v, "Root.md"),
            root_winner,
            "[{label}] min-UUID wins Root.md"
        );

        // folder collision at team/: ONE survivor folder node holding the union; the
        // surfaced team/Plan.md collision resolves by min-UUID + a conflict file.
        assert_eq!(
            folder_nodes_at(v, "team"),
            1,
            "[{label}] one surviving team/ folder"
        );
        let plan_winner = m.plan_a.min(m.plan_b);
        let plan_loser = m.plan_a.max(m.plan_b);
        let plan_conflict = conflict_path_for("team/Plan.md", &plan_loser);
        assert_eq!(
            uuid_at(v, "team/Plan.md"),
            plan_winner,
            "[{label}] min-UUID wins team/Plan.md"
        );

        // file-vs-folder at docs.md: the folder wins, D's file relocated to
        // docs.md/docs.md (no collision there — the only occupant); the folder's own
        // child keeps its UUID (the merge didn't disturb the unioned child's identity).
        assert_eq!(
            folder_nodes_at(v, "docs.md"),
            1,
            "[{label}] docs.md/ folder survives"
        );
        assert_eq!(
            uuid_at(v, "docs.md/x.md"),
            m.docs_folder_child_uuid,
            "[{label}] the folder's own child keeps its UUID through the merge"
        );
        assert_eq!(
            uuid_at(v, "docs.md/docs.md"),
            m.docs_file,
            "[{label}] the file relocated INSIDE the folder, UUID preserved"
        );

        // folder-delete + concurrent add: old/added.md rescued (present + intact),
        // old/gone.md stays deleted.
        assert_eq!(
            read_md_str(fs, "old/added.md").await,
            "# added\n\nADDED body important",
            "[{label}] the concurrent add was rescued intact"
        );

        // The EXACT final file set — every survivor + conflict + relocation + rescue,
        // nothing stray, nothing lost.
        let mut expected = BTreeSet::from([
            "Root.md".to_string(),
            "team/Plan.md".to_string(),
            "team/a.md".to_string(),
            "team/b.md".to_string(),
            plan_conflict,
            "docs.md/x.md".to_string(),
            "docs.md/docs.md".to_string(),
            "old/added.md".to_string(),
        ]);
        for c in root_conflicts {
            expected.insert(c);
        }
        assert_eq!(
            alive_md_paths(v).await,
            expected,
            "[{label}] exact converged file set across all four collisions"
        );
        assert!(
            !alive_md_paths(v).await.contains("old/gone.md"),
            "[{label}] the genuinely-deleted old/gone.md stays gone"
        );
    }

    /// The whole battery under the FIXED-order pump: stage the mixed mesh, pump to
    /// quiescence in fixed pair order, and assert every replica resolved all four
    /// collisions identically + converged byte-for-byte.
    #[tokio::test]
    async fn mixed_collision_mesh_converges_under_fixed_order() {
        let m = stage().await;
        let refs: Vec<&V> = m.replicas.iter().map(|(v, _)| v).collect();
        pump_to_quiescence(&refs).await;

        for (i, (v, fs)) in m.replicas.iter().enumerate() {
            assert_mesh_resolved(&m, &format!("R{i}"), v, fs).await;
        }
        let pairs: Vec<(&V, &Fs)> = m.replicas.iter().map(|(v, fs)| (v, fs)).collect();
        assert_all_converged(&pairs).await;
    }

    /// The SAME battery under a SHUFFLED pump order (several seeds → several distinct
    /// integration orders): a fresh mesh per seed, pumped in shuffled pair order, must
    /// reach the SAME byte-identical resolution as the fixed-order run. Order-independent
    /// convergence for the whole P2 surface (INV-5.3(c) / INV-4) — the capstone
    /// determinism proof.
    #[tokio::test]
    async fn mixed_collision_mesh_converges_under_shuffled_order() {
        for seed in [3u64, 17, 99, 2718] {
            let m = stage().await;
            let refs: Vec<&V> = m.replicas.iter().map(|(v, _)| v).collect();
            pump_to_quiescence_shuffled(&refs, seed).await;

            for (i, (v, fs)) in m.replicas.iter().enumerate() {
                assert_mesh_resolved(&m, &format!("seed {seed} R{i}"), v, fs).await;
            }
            let pairs: Vec<(&V, &Fs)> = m.replicas.iter().map(|(v, fs)| (v, fs)).collect();
            assert_all_converged(&pairs).await;
        }
    }
}

// ============================ termination under induced collisions ============================
//
// The cascade's lowest component (file-collision-sites) is NOT governed by a per-step
// lexicographic decrease — a loser renamed onto an occupied path re-surfaces as a NEW
// collision (INV-5.3(b)). Termination rests on INV-5.2's bounded naming: each loser's
// conflict path embeds its OWN full UUID, so it collides with each pre-existing live path
// at most once → the rename chain is bounded. This pins the worst case: a conflict-file
// rename landing on a path that ALSO has multiple colliders terminates and converges.

mod termination_under_induced_collisions {
    use super::*;

    /// A loser's conflict path ALREADY holds a live document, AND a second loser's
    /// conflict path coincides with it — the cascade must resolve the pile-up at the
    /// contested conflict path in the same pass, terminate (no `MAX_STEPS` panic), and
    /// converge identically both directions, with every document preserved (INV-3).
    ///
    /// Construction: a base collision at `Note.md` between A and B; the base loser's
    /// conflict path is pre-occupied by a THIRD live document; and the base loser itself
    /// then contends for that occupied conflict path. The cascade resolves the base
    /// collision, the loser's rename lands on the occupied path, and that residual
    /// collision is resolved by the same deterministic rules — a bounded transitive
    /// chain (`pump_to_quiescence`'s round cap + the resolver's `MAX_STEPS` guard would
    /// catch a non-terminating bug by panicking instead of hanging).
    #[tokio::test]
    async fn conflict_rename_onto_occupied_path_terminates_and_converges() {
        for forward in [true, false] {
            let (a, b, fs_a, fs_b) = two_vaults().await;

            // Base collision at Note.md.
            write_and_index(&a, &fs_a, "Note.md", "A body\n").await;
            write_and_index(&b, &fs_b, "Note.md", "B body\n").await;
            let ua = uuid_at(&a, "Note.md");
            let ub = uuid_at(&b, "Note.md");
            let base_loser = ua.max(ub);

            // Pre-occupy the EXACT path the base loser will be renamed to with a third
            // live document (synced in, so it is a real peer doc, not a local fixture).
            let contested = conflict_path_for("Note.md", &base_loser);
            write_and_index(&a, &fs_a, &contested, "occupant body\n").await;
            let occupant = uuid_at(&a, &contested);

            if forward {
                pump_to_quiescence(&[&a, &b]).await;
            } else {
                pump_to_quiescence(&[&b, &a]).await;
            }

            let label = if forward { "A->B" } else { "B->A" };

            // Base survivor wins Note.md; at the contested conflict path the min-UUID of
            // {base_loser, occupant} wins and the other gets a FURTHER full-UUID conflict
            // file off it — the bounded transitive resolution.
            let base_survivor = ua.min(ub);
            let contested_winner = base_loser.min(occupant);
            let contested_loser = base_loser.max(occupant);
            let further = conflict_path_for(&contested, &contested_loser);

            assert_eq!(
                uuid_at(&a, "Note.md"),
                base_survivor,
                "[{label}] base survivor on A"
            );
            assert_eq!(
                uuid_at(&a, &contested),
                contested_winner,
                "[{label}] min-UUID wins the contested conflict path"
            );
            assert_eq!(
                uuid_at(&a, &further),
                contested_loser,
                "[{label}] the displaced loser lands at a further full-UUID conflict file"
            );

            // All THREE documents survive somewhere on both replicas — nothing dropped or
            // overwritten in the pile-up (INV-3). The pass terminated (no panic above).
            for (who, v) in [("A", &a), ("B", &b)] {
                let placed: BTreeSet<Uuid> = {
                    let mut s = BTreeSet::new();
                    for p in alive_md_paths(v).await {
                        s.insert(uuid_at(v, &p));
                    }
                    s
                };
                assert_eq!(
                    placed,
                    BTreeSet::from([base_survivor, base_loser, occupant]),
                    "[{label}] {who}: all three documents survive at distinct paths"
                );
            }
            assert_converged(&a, &fs_a, &b, &fs_b).await;
        }
    }
}
