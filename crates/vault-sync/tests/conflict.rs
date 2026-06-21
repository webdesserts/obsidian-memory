//! Integration tests for the structural-conflict cascade (Chunk P2b).
//!
//! These exercise the END-TO-END conflict resolution a vault consumer observes: two
//! (or three) replicas independently create different things at the same display
//! path, sync, and the library converges every replica to the SAME deterministic
//! resolution — a single survivor at the path (min-UUID) plus a `(conflict <uuid>)`
//! file for every kept loser, with no content ever silently dropped (INV-3/INV-5).
//!
//! The cascade fires once per `process_message` on the fully-merged Index + content
//! state (INV-5.0/5.3). A colliding peer's content lands in the same message's
//! document updates (after the Index delta, INV-8), so the cascade fires after that
//! content is present; that is why a single `sync_both_ways` (one exchange each
//! direction) is enough to converge a freshly-created collision.
//!
//! Everything runs against `InMemoryFs` — no test touches a real on-disk vault, and
//! there is no iroh/transport here (the seam is bytes, pumped by the harness).

mod common;
use common::*;
use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;
use uuid::Uuid;
use vault_sync::{DocId, FileSystem, InMemoryFs, SyncMessage, Vault, content_doc_path};

// ============================ shared assertion helpers ============================

/// The set of `.md` files a vault currently tracks (survivors + conflict files),
/// for exact-set assertions ("exactly these files exist, no stray intermediate ones").
async fn md_files(vault: &V) -> BTreeSet<String> {
    vault.list_files().await.unwrap().into_iter().collect()
}

/// The conflict-file path the cascade renames a loser to: `<stem> (conflict <uuid>)<ext>`
/// in the same parent dir — the full 36-char UUID makes it collision-proof (INV-5.2).
fn conflict_path(original: &str, loser: &Uuid) -> String {
    vault_sync::conflict_name(original, loser)
}

/// Read a `.md` file as a String (panics if absent).
async fn read_md_str(fs: &Fs, path: &str) -> String {
    String::from_utf8(read_md(fs, path).await).unwrap()
}

/// Build three empty in-memory vaults (A/B/C, authored 1/2/3) with their retained
/// filesystems — the N-group counterpart to `two_vaults`.
async fn three_vaults() -> (V, V, V, Fs, Fs, Fs) {
    let fs_a = Arc::new(InMemoryFs::new());
    let fs_b = Arc::new(InMemoryFs::new());
    let fs_c = Arc::new(InMemoryFs::new());
    let a = Vault::init(Arc::clone(&fs_a), author(1)).await.unwrap();
    let b = Vault::init(Arc::clone(&fs_b), author(2)).await.unwrap();
    let c = Vault::init(Arc::clone(&fs_c), author(3)).await.unwrap();
    (a, b, c, fs_a, fs_b, fs_c)
}

/// Assert both replicas agree on the UUID living at `path` and that it equals `want`.
fn assert_survivor(a: &V, b: &V, path: &str, want: Uuid) {
    assert_eq!(uuid_at(a, path), want, "A's survivor at {path}");
    assert_eq!(uuid_at(b, path), want, "B's survivor at {path}");
}

// ============================ AC-INV-5 — the three rules ============================
//
// Each rule is checked on BOTH replicas and in BOTH sync directions (the resolution
// must be symmetric — no locality, INV-5 determinism guarantee).

mod ac_inv_5_three_rules {
    use super::*;

    /// Rule 1 — identical content collapses to ONE survivor, byte-identical on both
    /// replicas. Two replicas independently create the SAME content at one path
    /// (distinct UUIDs, identical materialized markdown). The cascade collapses the
    /// max-UUID loser; the min-UUID survivor keeps the path. No conflict file is
    /// created — there was nothing to keep apart.
    #[tokio::test]
    async fn rule1_identical_content_collapses_to_one_survivor() {
        let (a, b, fs_a, fs_b) = two_vaults().await;

        // Byte-identical content on both sides → identical materialized markdown.
        let content = "---\ntitle: Shared\n---\n\n# Heading\n\nSame body.\n";
        write_and_index(&a, &fs_a, "Note.md", content).await;
        write_and_index(&b, &fs_b, "Note.md", content).await;

        let ua = uuid_at(&a, "Note.md");
        let ub = uuid_at(&b, "Note.md");
        let survivor = ua.min(ub);
        let loser = ua.max(ub);

        sync_both_ways(&a, &b).await;

        // One survivor at the path on both replicas; the loser is gone.
        assert_survivor(&a, &b, "Note.md", survivor);
        let loser_conflict = conflict_path("Note.md", &loser);
        assert!(
            !fs_a.exists(&loser_conflict).await.unwrap(),
            "no conflict file for identical content (A)"
        );
        assert!(
            !fs_b.exists(&loser_conflict).await.unwrap(),
            "no conflict file for identical content (B)"
        );

        // Exactly one `.md` survives — the loser's node and file are collapsed away.
        assert_eq!(
            md_files(&a).await,
            BTreeSet::from(["Note.md".to_string()]),
            "A holds exactly the one survivor file"
        );
        assert_eq!(
            md_files(&a).await,
            md_files(&b).await,
            "both replicas agree"
        );

        // The survivor's content is intact and byte-identical across replicas.
        assert_eq!(
            read_md_str(&fs_a, "Note.md").await,
            read_md_str(&fs_b, "Note.md").await,
            "the survivor materializes byte-identically on both replicas"
        );
    }

    /// Rule 2 — an empty stub loses to real content. One replica creates a blank
    /// document at the path, the other creates a real one. The empty doc has no
    /// content to lose, so it is dropped; the non-empty doc survives at the path. No
    /// conflict file (rule 2 never keeps the empty loser).
    #[tokio::test]
    async fn rule2_empty_loses_to_non_empty() {
        let (a, b, fs_a, fs_b) = two_vaults().await;

        // A writes a real note; B writes a blank stub at the same path.
        write_and_index(&a, &fs_a, "Note.md", "# Real\n\nActual content.\n").await;
        write_and_index(&b, &fs_b, "Note.md", "\n").await;

        let ua = uuid_at(&a, "Note.md");
        let ub = uuid_at(&b, "Note.md");

        sync_both_ways(&a, &b).await;

        // The non-empty document (A's) survives on BOTH replicas, regardless of which
        // UUID is smaller — emptiness beats the min-UUID tiebreak.
        assert_survivor(&a, &b, "Note.md", ua);
        assert!(
            ua != ub,
            "precondition: the two docs have distinct UUIDs (a real collision)"
        );

        // No conflict file for the dropped empty stub on either side.
        let empty_conflict = conflict_path("Note.md", &ub);
        assert!(!fs_a.exists(&empty_conflict).await.unwrap());
        assert!(!fs_b.exists(&empty_conflict).await.unwrap());
        assert_eq!(
            md_files(&a).await,
            BTreeSet::from(["Note.md".to_string()]),
            "only the non-empty survivor remains (A)"
        );
        assert_eq!(md_files(&a).await, md_files(&b).await);

        // The surviving content is the real note, present and intact on both replicas.
        assert!(
            read_md_str(&fs_a, "Note.md")
                .await
                .contains("Actual content.")
        );
        assert!(
            read_md_str(&fs_b, "Note.md")
                .await
                .contains("Actual content.")
        );
    }

    /// Rule 3 — two distinct non-empty docs are BOTH kept: the min-UUID survives at
    /// the path, the loser is renamed to a full-UUID conflict file, and BOTH bodies
    /// are present on BOTH replicas (INV-3 — no silent loss). Checked in both sync
    /// directions to prove the resolution is symmetric.
    #[tokio::test]
    async fn rule3_distinct_non_empty_keeps_both_min_uuid_survives() {
        for direction in ["a_first", "b_first"] {
            let (a, b, fs_a, fs_b) = two_vaults().await;

            write_and_index(&a, &fs_a, "projects/Note.md", "# A\n\nAlpha body.\n").await;
            write_and_index(&b, &fs_b, "projects/Note.md", "# B\n\nBeta body.\n").await;

            let ua = uuid_at(&a, "projects/Note.md");
            let ub = uuid_at(&b, "projects/Note.md");
            let survivor = ua.min(ub);
            let loser = ua.max(ub);

            // Drive the handshake from one side or the other — the outcome must match.
            match direction {
                "a_first" => sync_both_ways(&a, &b).await,
                _ => sync_both_ways(&b, &a).await,
            }

            // Min-UUID wins the path on both replicas.
            assert_survivor(&a, &b, "projects/Note.md", survivor);

            // The loser lives at a conflict file (same parent dir, FULL UUID suffix)
            // on both replicas.
            let loser_conflict = conflict_path("projects/Note.md", &loser);
            assert!(
                loser_conflict.contains(&loser.to_string()),
                "[{direction}] conflict path embeds the full 36-char UUID"
            );
            assert_eq!(loser.to_string().len(), 36);
            assert!(
                fs_a.exists(&loser_conflict).await.unwrap(),
                "[{direction}] A keeps the loser at its conflict file"
            );
            assert!(
                fs_b.exists(&loser_conflict).await.unwrap(),
                "[{direction}] B keeps the loser at its conflict file"
            );

            // BOTH bodies survive — the cascade kept both documents (INV-3). The
            // survivor body and the loser body are each materialized somewhere on
            // each replica.
            let want_survivor_body = if survivor == ua {
                "Alpha body."
            } else {
                "Beta body."
            };
            let want_loser_body = if loser == ua {
                "Alpha body."
            } else {
                "Beta body."
            };
            for (label, fs) in [("A", &fs_a), ("B", &fs_b)] {
                let at_path = read_md_str(fs, "projects/Note.md").await;
                let at_conflict = read_md_str(fs, &loser_conflict).await;
                assert!(
                    at_path.contains(want_survivor_body),
                    "[{direction}] {label}: survivor body at the path"
                );
                assert!(
                    at_conflict.contains(want_loser_body),
                    "[{direction}] {label}: loser body at the conflict file"
                );
            }

            // Exactly the survivor + the one conflict file exist — no stray files.
            let expected = BTreeSet::from(["projects/Note.md".to_string(), loser_conflict.clone()]);
            assert_eq!(md_files(&a).await, expected, "[{direction}] A's file set");
            assert_eq!(md_files(&b).await, expected, "[{direction}] B's file set");
        }
    }
}

// ============================ AC-INV-3 — no silent loss ============================

mod ac_inv_3_no_silent_loss {
    use super::*;

    /// The old path-keyed sync resolved a distinct-UUID same-path collision by
    /// silently overwriting the loser (machine-local-mtime latest-wins) — an INV-3
    /// violation. Under UUID keying + the cascade, that same scenario now KEEPS both:
    /// a conflict file appears, and every pre-sync body is still present afterwards.
    /// This is the regression guard against any return of silent latest-wins.
    #[tokio::test]
    async fn distinct_collision_creates_conflict_file_no_body_dropped() {
        let (a, b, fs_a, fs_b) = two_vaults().await;

        // Two genuinely different bodies at one path — what would have been silently
        // overwritten before.
        write_and_index(&a, &fs_a, "Note.md", "important draft from A\n").await;
        write_and_index(&b, &fs_b, "Note.md", "different draft from B\n").await;
        let ua = uuid_at(&a, "Note.md");
        let ub = uuid_at(&b, "Note.md");

        sync_both_ways(&a, &b).await;

        // A conflict file now exists (no silent overwrite). The loser's body is in it.
        let loser = ua.max(ub);
        let loser_conflict = conflict_path("Note.md", &loser);
        assert!(
            fs_a.exists(&loser_conflict).await.unwrap()
                && fs_b.exists(&loser_conflict).await.unwrap(),
            "the collision produced a conflict file instead of a silent overwrite"
        );

        // BOTH pre-sync bodies are present after the sync (nothing dropped). Collect
        // every body materialized on A and assert both originals survive.
        let mut bodies = Vec::new();
        for path in md_files(&a).await {
            bodies.push(read_md_str(&fs_a, &path).await);
        }
        let all = bodies.join("\n");
        assert!(
            all.contains("important draft from A"),
            "A's original body survived"
        );
        assert!(
            all.contains("different draft from B"),
            "B's original body survived"
        );
    }
}

// ============================ AC-INV-5-NGROUP — whole-group N≥2 ============================

mod ac_inv_5_ngroup {
    use super::*;

    /// THREE replicas each create a DISTINCT non-empty document at one path. The
    /// cascade resolves the whole group at once (INV-5.1): the GLOBAL min-UUID wins
    /// the path, every other lands at its own `(conflict <full-uuid>)` file —
    /// identically on all three replicas. The 3-way group is the proof the cascade
    /// fires on the fully-merged state, NOT iteratively pairwise (a pairwise fold
    /// could pick a different survivor depending on order and diverge).
    #[tokio::test]
    async fn three_distinct_docs_global_min_wins_others_get_conflict_files() {
        let (a, b, c, fs_a, fs_b, fs_c) = three_vaults().await;

        write_and_index(&a, &fs_a, "Note.md", "# A\n\nAlpha.\n").await;
        write_and_index(&b, &fs_b, "Note.md", "# B\n\nBeta.\n").await;
        write_and_index(&c, &fs_c, "Note.md", "# C\n\nGamma.\n").await;

        let ua = uuid_at(&a, "Note.md");
        let ub = uuid_at(&b, "Note.md");
        let uc = uuid_at(&c, "Note.md");
        let survivor = ua.min(ub).min(uc);
        let losers: Vec<Uuid> = [ua, ub, uc]
            .into_iter()
            .filter(|u| *u != survivor)
            .collect();

        // Pump the full mesh to quiescence — every ordered pair exchanges until nothing
        // changes. The convergence is what proves whole-group, merged-state resolution.
        pump_to_quiescence(&[&a, &b, &c]).await;

        // Every replica agrees: global min-UUID at the path, each other at its own
        // full-UUID conflict file.
        let mut expected = BTreeSet::from(["Note.md".to_string()]);
        for loser in &losers {
            expected.insert(conflict_path("Note.md", loser));
        }
        for (label, vault) in [("A", &a), ("B", &b), ("C", &c)] {
            assert_eq!(
                uuid_at(vault, "Note.md"),
                survivor,
                "{label}: global min-UUID survives the path"
            );
            assert_eq!(
                md_files(vault).await,
                expected,
                "{label}: survivor + one conflict file per loser, nothing else"
            );
        }

        // No body lost: the three originals are each present on replica A.
        let mut bodies = Vec::new();
        for path in md_files(&a).await {
            bodies.push(read_md_str(&fs_a, &path).await);
        }
        let all = bodies.join("\n");
        for needle in ["Alpha.", "Beta.", "Gamma."] {
            assert!(all.contains(needle), "the {needle} body survived (INV-3)");
        }
    }

    /// The whole-group rule must pick the GLOBAL minimum even when a naive pairwise
    /// fold (visited in arrival order) would commit to a different survivor. Three
    /// replicas where the global-min UUID belongs to the replica that syncs LAST —
    /// the global min must still win on every replica, never appear as a conflict
    /// file. (The integration analogue of P2a's `ngroup_picks_global_min_not_pairwise`.)
    #[tokio::test]
    async fn global_min_wins_even_when_it_arrives_last() {
        // Keep re-rolling identities until the global-min UUID belongs to C (the
        // replica we sync last), so a pairwise fold over A,B would be tempted to keep
        // an A/B survivor. UUIDs are random per `write_and_index`, so loop until the
        // arrangement holds (cheap; converges in a couple of tries).
        let mut attempt = 0;
        loop {
            attempt += 1;
            assert!(
                attempt < 50,
                "could not arrange global-min-on-C in 50 tries"
            );

            let (a, b, c, fs_a, fs_b, fs_c) = three_vaults().await;

            write_and_index(&a, &fs_a, "Note.md", "# A\n\nAlpha.\n").await;
            write_and_index(&b, &fs_b, "Note.md", "# B\n\nBeta.\n").await;
            write_and_index(&c, &fs_c, "Note.md", "# C\n\nGamma.\n").await;

            let ua = uuid_at(&a, "Note.md");
            let ub = uuid_at(&b, "Note.md");
            let uc = uuid_at(&c, "Note.md");
            if uc != ua.min(ub).min(uc) {
                continue; // global min is not C's — re-roll
            }

            // Sync A↔B first (so a pairwise resolver would settle on min(ua,ub)), then
            // bring C in last, then pump to quiescence.
            sync_both_ways(&a, &b).await;
            sync_both_ways(&a, &c).await;
            sync_both_ways(&b, &c).await;
            pump_to_quiescence(&[&a, &b, &c]).await;

            // C's UUID (the global min) must win the path everywhere, never be a loser.
            for (label, vault) in [("A", &a), ("B", &b), ("C", &c)] {
                assert_eq!(
                    uuid_at(vault, "Note.md"),
                    uc,
                    "{label}: the global-min UUID (C's, synced last) wins the path"
                );
                let c_conflict = conflict_path("Note.md", &uc);
                assert!(
                    !fs_a.exists(&c_conflict).await.unwrap(),
                    "{label}: the global min never lands at a conflict file"
                );
            }
            return;
        }
    }
}

// ============================ AC-INV-5-BATCH — fires once, merged state ============================

mod ac_inv_5_batch {
    use super::*;

    /// MULTIPLE independent collisions in one merged delta resolve consistently and
    /// converge regardless of sync direction — the cascade fires ONCE on the
    /// fully-merged state, resolving every collision together, not per-op. Two
    /// replicas create distinct docs at TWO different paths; after converging, each
    /// path has its own deterministic survivor + conflict file, identical on both
    /// replicas and in both directions. The exact final file set (no stray
    /// intermediate-named conflict files) is the no-instrument proof the pass did not
    /// fire on an intermediate per-op state.
    #[tokio::test]
    async fn multiple_collisions_resolve_consistently_in_any_direction() {
        for direction in ["a_first", "b_first"] {
            let (a, b, fs_a, fs_b) = two_vaults().await;

            // Two distinct collisions in one batch: Note1.md and dir/Note2.md.
            write_and_index(&a, &fs_a, "Note1.md", "# A1\n\nA-one.\n").await;
            write_and_index(&a, &fs_a, "dir/Note2.md", "# A2\n\nA-two.\n").await;
            write_and_index(&b, &fs_b, "Note1.md", "# B1\n\nB-one.\n").await;
            write_and_index(&b, &fs_b, "dir/Note2.md", "# B2\n\nB-two.\n").await;

            let s1 = uuid_at(&a, "Note1.md").min(uuid_at(&b, "Note1.md"));
            let l1 = uuid_at(&a, "Note1.md").max(uuid_at(&b, "Note1.md"));
            let s2 = uuid_at(&a, "dir/Note2.md").min(uuid_at(&b, "dir/Note2.md"));
            let l2 = uuid_at(&a, "dir/Note2.md").max(uuid_at(&b, "dir/Note2.md"));

            match direction {
                "a_first" => sync_both_ways(&a, &b).await,
                _ => sync_both_ways(&b, &a).await,
            }

            // Both collisions resolved to their own min-UUID survivor + conflict file.
            assert_survivor(&a, &b, "Note1.md", s1);
            assert_survivor(&a, &b, "dir/Note2.md", s2);

            let expected = BTreeSet::from([
                "Note1.md".to_string(),
                "dir/Note2.md".to_string(),
                conflict_path("Note1.md", &l1),
                conflict_path("dir/Note2.md", &l2),
            ]);
            // EXACTLY these four files on both replicas — no stray conflict file from a
            // per-op intermediate resolution (which would leave a stale name behind).
            assert_eq!(
                md_files(&a).await,
                expected,
                "[{direction}] A: exactly the survivors + their conflict files"
            );
            assert_eq!(
                md_files(&b).await,
                expected,
                "[{direction}] B: same exact file set (converged, both directions)"
            );
        }
    }

    /// The two directions of the BATCH scenario produce the IDENTICAL final file set —
    /// pinned explicitly so a direction-dependent (non-merged-state) resolution would
    /// fail. Drives the same two-collision setup with the SAME identities both ways by
    /// staging the collision, snapshotting the resolution from an A-first run, and
    /// asserting a B-first run lands on the same paths.
    #[tokio::test]
    async fn both_directions_converge_to_identical_paths() {
        // A-first run.
        let (a1, b1, fa1, fb1) = two_vaults().await;
        write_and_index(&a1, &fa1, "x/N.md", "alpha\n").await;
        write_and_index(&b1, &fb1, "x/N.md", "beta\n").await;
        sync_both_ways(&a1, &b1).await;
        let a_first_files = md_files(&a1).await;
        // Both sides identical within the A-first run.
        assert_eq!(a_first_files, md_files(&b1).await);

        // B-first run with fresh identities — assert it converges to the SAME shape:
        // one survivor at x/N.md plus exactly one conflict file.
        let (a2, b2, fa2, fb2) = two_vaults().await;
        write_and_index(&a2, &fa2, "x/N.md", "alpha\n").await;
        write_and_index(&b2, &fb2, "x/N.md", "beta\n").await;
        sync_both_ways(&b2, &a2).await;
        let b_first_files = md_files(&a2).await;
        assert_eq!(b_first_files, md_files(&b2).await);

        // Same structural shape regardless of direction: x/N.md + exactly one
        // conflict file in the same dir.
        for files in [&a_first_files, &b_first_files] {
            assert!(files.contains("x/N.md"), "survivor at the path");
            assert_eq!(files.len(), 2, "exactly survivor + one conflict file");
            let conflict = files.iter().find(|p| p.as_str() != "x/N.md").unwrap();
            assert!(
                conflict.starts_with("x/N (conflict ") && conflict.ends_with(").md"),
                "the loser lands at a full-UUID conflict file in the same dir: {conflict}"
            );
        }
    }
}

// ============================ AC-INV-5-NAMECOLLIDE — conflict-file self-collision ============================

mod ac_inv_5_namecollide {
    use super::*;

    /// A loser's conflict path ALREADY holds a live user document. The residual
    /// collision resolves deterministically within the same cascade (INV-5.2): the
    /// min-UUID of {loser, pre-existing occupant} wins the conflict path, the other
    /// gets a FURTHER full-UUID conflict file off it. Nothing is overwritten or lost,
    /// and both sync directions agree.
    ///
    /// Construction: replica B pre-creates a file at the EXACT path A's loser would be
    /// renamed to. To know that path up front we must know A's loser UUID, so we stage
    /// the base collision, read the loser UUID, then plant the occupant — all before
    /// the cascade runs (the occupant is itself synced in, so it is a real third
    /// document, not a local-only fixture).
    #[tokio::test]
    async fn loser_conflict_path_already_occupied_resolves_deterministically() {
        let (a, b, fs_a, fs_b) = two_vaults().await;

        // Base collision at Note.md: A and B each create a distinct doc.
        write_and_index(&a, &fs_a, "Note.md", "A body\n").await;
        write_and_index(&b, &fs_b, "Note.md", "B body\n").await;
        let ua = uuid_at(&a, "Note.md");
        let ub = uuid_at(&b, "Note.md");
        let base_loser = ua.max(ub);

        // The path the base loser will be renamed to.
        let contested = conflict_path("Note.md", &base_loser);

        // Plant a THIRD live document at exactly that contested conflict path on A
        // (it will sync to B too). This is the pre-existing occupant.
        write_and_index(&a, &fs_a, &contested, "occupant body\n").await;
        let occupant = uuid_at(&a, &contested);

        // Converge everything.
        pump_to_quiescence(&[&a, &b]).await;

        // Note.md → the base survivor (min of ua, ub) on both replicas.
        let base_survivor = ua.min(ub);
        assert_survivor(&a, &b, "Note.md", base_survivor);

        // The contested conflict path → min(base_loser, occupant); the other is pushed
        // to a FURTHER conflict file derived from its own UUID.
        let contested_winner = base_loser.min(occupant);
        let contested_loser = base_loser.max(occupant);
        assert_survivor(&a, &b, &contested, contested_winner);

        let further = conflict_path(&contested, &contested_loser);
        assert!(
            fs_a.exists(&further).await.unwrap() && fs_b.exists(&further).await.unwrap(),
            "the displaced document lands at a further full-UUID conflict file"
        );

        // All THREE documents survive somewhere — nothing dropped or overwritten
        // (INV-3). Each UUID resolves to a live node on both replicas.
        for vault in [&a, &b] {
            let placed: BTreeSet<Uuid> = md_files(vault)
                .await
                .iter()
                .map(|p| uuid_at(vault, p))
                .collect();
            assert_eq!(
                placed,
                BTreeSet::from([base_survivor, base_loser, occupant]),
                "all three documents survive at distinct paths"
            );
        }

        // Both replicas have the identical file set (deterministic, both directions).
        assert_eq!(md_files(&a).await, md_files(&b).await);
    }
}

// ============================ DP-5 — interplay with move/delete detection ============================

mod dp5_move_delete_interplay {
    use super::*;

    /// The subtle interplay the cascade must NOT break: a collision where THIS
    /// replica's PRE-EXISTING node is the loser (the imported node has the smaller
    /// UUID). The pre-existing loser is renamed to its conflict path EXACTLY ONCE — it
    /// must NOT be additionally mis-detected as a "move" by the apply_index move/delete
    /// pass (which runs against pre-import snapshots) and double-materialized — and the
    /// imported survivor takes the original path.
    ///
    /// This is the DP-5 risk made concrete. The cascade fires after the move/delete
    /// detection has already consumed its pre-import snapshots, and the survivor (not
    /// the loser) ends up at the loser's old path, so the move-detection guard skips
    /// it. We assert the end state is clean: survivor at the path, loser at exactly one
    /// conflict file, no duplicate/stray file.
    #[tokio::test]
    async fn local_loser_is_renamed_once_survivor_takes_path() {
        // Re-roll until the LOCAL replica (B, which receives) is the LOSER — i.e. the
        // remote replica A's UUID is smaller. Then on B the pre-existing node loses to
        // the imported one, which is the exact DP-5 case.
        let mut attempt = 0;
        loop {
            attempt += 1;
            assert!(attempt < 50, "could not arrange local-loser in 50 tries");

            let (a, b, fs_a, fs_b) = two_vaults().await;
            write_and_index(&a, &fs_a, "Note.md", "remote body\n").await;
            write_and_index(&b, &fs_b, "Note.md", "local body\n").await;

            let ua = uuid_at(&a, "Note.md"); // remote (imported into B)
            let ub = uuid_at(&b, "Note.md"); // local pre-existing on B
            if ua >= ub {
                continue; // need the imported (A) node to be the survivor on B
            }

            // A initiates: B imports A's node (smaller UUID → survivor) while B's own
            // node (the loser) is pre-existing — the DP-5 shape.
            sync_both_ways(&a, &b).await;

            // On B: the imported survivor (A's UUID) holds the original path.
            assert_eq!(
                uuid_at(&b, "Note.md"),
                ua,
                "the imported (smaller-UUID) node wins the original path on B"
            );
            // B's pre-existing node (the loser) is at its conflict file — and ONLY
            // there (not also stranded at the old path / not duplicated).
            let loser_conflict = conflict_path("Note.md", &ub);
            assert!(
                fs_b.exists(&loser_conflict).await.unwrap(),
                "B's pre-existing loser is renamed to its conflict file"
            );

            // EXACTLY two files on B: the survivor at the path + the one conflict file.
            // A double-materialized loser (the DP-5 bug) would add a third stray file.
            assert_eq!(
                md_files(&b).await,
                BTreeSet::from(["Note.md".to_string(), loser_conflict.clone()]),
                "no double-materialization: exactly survivor + one conflict file on B"
            );

            // Both bodies survive and are in the right places on B.
            assert!(
                read_md_str(&fs_b, "Note.md").await.contains("remote body"),
                "survivor body (A's) at the path on B"
            );
            assert!(
                read_md_str(&fs_b, &loser_conflict)
                    .await
                    .contains("local body"),
                "loser body (B's) at the conflict file on B"
            );

            // A converged identically — same survivor, same single conflict file.
            assert_eq!(uuid_at(&a, "Note.md"), ua, "A agrees on the survivor");
            assert_eq!(
                md_files(&a).await,
                BTreeSet::from(["Note.md".to_string(), loser_conflict.clone()]),
                "A has the same exact file set"
            );
            return;
        }
    }

    /// The cascade must not regress a genuine move: an actual relocate (not a
    /// collision) still propagates as a zero-content move. A creates and moves a
    /// document with NO competing node at the destination — the move-detection path
    /// handles it, the cascade is a no-op (no collision), and the document ends up at
    /// the new path on B with its UUID intact. (A focused guard that the cascade's
    /// firing did not perturb the move/delete tail for the non-collision case.)
    #[tokio::test]
    async fn genuine_move_without_collision_still_propagates() {
        let (a, b, fs_a, fs_b) = two_vaults().await;

        write_and_index(&a, &fs_a, "notes/topic.md", "Body text here.\n").await;
        full_sync(&a, &b).await;
        let uuid = uuid_at(&b, "notes/topic.md");

        // A moves it; no node competes at the destination → no collision.
        move_file(&a, &fs_a, "notes/topic.md", "archive/topic.md").await;
        sync_both_ways(&a, &b).await;

        // B converged on the move, same UUID, old path vacated, new path materialized.
        assert!(
            b.index().node_for_path("notes/topic.md").is_none(),
            "old path vacated on B"
        );
        assert_eq!(
            uuid_at(&b, "archive/topic.md"),
            uuid,
            "moved document keeps its UUID on B"
        );
        assert!(
            read_md_str(&fs_b, "archive/topic.md")
                .await
                .contains("Body text here."),
            "the moved .md is materialized at the new path on B"
        );
        // No accidental conflict files anywhere.
        assert_eq!(
            md_files(&b).await,
            BTreeSet::from(["archive/topic.md".to_string()]),
            "exactly the moved file, no stray conflict files"
        );
    }
}

// ============ DP-6 — a live-push DocUpdate completes a Flow-2-gated collision ============

mod dp6_live_push_completes_collision {
    use super::*;

    /// Deliver ONLY `sender`'s Index snapshot to `receiver` (no document content),
    /// the way a partial/Index-only exchange does. The colliding node lands but its
    /// body stays Flow-2-gated, so the cascade cannot yet resolve it (DP-6 omits a
    /// content-less collider from its view). Mirrors the Index-only delivery the
    /// reconcile suite uses to stage a node-without-content state.
    async fn deliver_index_only(sender: &V, receiver: &V) {
        let index_only = SyncMessage::SyncResponse {
            index_updates: Some(sender.index().export_snapshot().unwrap()),
            document_updates: HashMap::new(),
        };
        receiver
            .process_message(&bincode::serialize(&index_only).unwrap())
            .await
            .unwrap();
    }

    /// The latency guarantee on the live-push path (INV-5 / DP-6): a collision whose
    /// other node arrived in a PRIOR Index sync — but whose CONTENT was Flow-2-gated —
    /// is resolved by the real-time `DocUpdate` that finally lands that content, within
    /// the SAME `process_message`, WITHOUT waiting for a subsequent full sync.
    ///
    /// Staging: B holds its own document at `Note.md`. A's competing node at `Note.md`
    /// is delivered to B via an Index-only message (its `.loro` does not arrive), so the
    /// collision exists structurally but the cascade omits A's content-less node and
    /// leaves it unresolved. Then A's content arrives as a lone `DocUpdate` — the
    /// live-push path. With the cascade wired into that arm, B converges immediately:
    /// the min-UUID survivor at the path, the loser at its conflict file.
    ///
    /// RED on the pre-fix code: the `DocUpdate` arm did not fire the cascade, so the
    /// collision lingered (no conflict file) until the next full sync.
    #[tokio::test]
    async fn live_push_resolves_a_collision_staged_by_a_prior_index_sync() {
        let (a, b, fs_a, fs_b) = two_vaults().await;

        // Both sides independently create a distinct, non-empty document at one path.
        write_and_index(&a, &fs_a, "Note.md", "# A\n\nAlpha body.\n").await;
        write_and_index(&b, &fs_b, "Note.md", "# B\n\nBeta body.\n").await;

        let ua = uuid_at(&a, "Note.md");
        let ub = uuid_at(&b, "Note.md");
        let survivor = ua.min(ub);
        let loser = ua.max(ub);

        // Step 1 — A's NODE lands on B (Index only), but its CONTENT does not. The
        // collision now exists in B's merged tree, yet the cascade cannot resolve it:
        // A's `.loro` is absent, so DP-6 omits A's node from the view.
        deliver_index_only(&a, &b).await;

        // Precondition: B sees both nodes alive at the path (a real, unresolved
        // collision), A's content is NOT on disk, and NO conflict file exists yet — the
        // cascade left the content-less collision pending.
        assert!(
            b.index().find_node_by_uuid(&ua).is_some()
                && b.index().find_node_by_uuid(&ub).is_some(),
            "both colliding nodes are alive on B after the Index-only delivery"
        );
        assert!(
            !fs_b.exists(&content_doc_path(&ua)).await.unwrap(),
            "A's content is still Flow-2-gated on B (no <uuid>.loro)"
        );
        let loser_conflict = conflict_path("Note.md", &loser);
        assert!(
            !fs_b.exists(&loser_conflict).await.unwrap(),
            "no conflict file yet — the content-less collision is unresolved"
        );

        // Step 2 — A's CONTENT arrives as a lone real-time DocUpdate (the live-push
        // path), NOT a full sync. This is the message that completes the collision.
        let push = a.prepare_doc_update(DocId(ua)).await.unwrap().unwrap();
        b.process_message(&push).await.unwrap();

        // The collision is resolved by the push alone — no subsequent full sync. The
        // min-UUID survivor holds the path; the loser is at its full-UUID conflict file;
        // both bodies are present (INV-3). This is exactly what the pre-fix `DocUpdate`
        // arm failed to do.
        assert_eq!(
            uuid_at(&b, "Note.md"),
            survivor,
            "the min-UUID survivor holds the path after the live push"
        );
        assert!(
            fs_b.exists(&loser_conflict).await.unwrap(),
            "the live push resolved the collision: the loser is at its conflict file"
        );
        assert_eq!(
            md_files(&b).await,
            BTreeSet::from(["Note.md".to_string(), loser_conflict.clone()]),
            "exactly the survivor + one conflict file — the collision is fully resolved"
        );

        // Both originals survive somewhere on B — nothing dropped.
        let mut bodies = Vec::new();
        for path in md_files(&b).await {
            bodies.push(read_md_str(&fs_b, &path).await);
        }
        let all = bodies.join("\n");
        assert!(
            all.contains("Alpha body."),
            "A's body survived the resolution"
        );
        assert!(
            all.contains("Beta body."),
            "B's body survived the resolution"
        );
    }
}

// ============ DP-5 — the swap branch keeps a re-occupied old path's .md ============

mod dp5_swap_keeps_reoccupied_path {
    use super::*;

    /// Two documents genuinely SWAP paths in one import batch: X moves P1→P2 while Y
    /// moves P2→P1. The move-detection's per-path `old_path_vacated = false` branch must
    /// KEEP each old path's `.md` (it is now the swapped-in document's), so BOTH files
    /// survive with their correct final bodies — nothing deleted.
    ///
    /// This exercises the swap branch directly, which neither existing DP-5 test reaches
    /// (the local-loser case has `new_path == old_path`; the genuine-move case always has
    /// `old_path_vacated == true`). If the `if mv.old_path_vacated` guard were inverted,
    /// processing one move would delete the other's just-materialized file — so the test
    /// fails (exactly one of the two files goes missing) unless the guard is correct.
    #[tokio::test]
    async fn two_docs_swapping_paths_both_survive_with_correct_bodies() {
        let (a, b, fs_a, fs_b) = two_vaults().await;

        // A creates X at one.md and Y at two.md, then syncs both to B (B gets both
        // nodes, both `.loro`s, and both `.md`s).
        write_and_index(&a, &fs_a, "one.md", "# X\n\nBody of X.\n").await;
        write_and_index(&a, &fs_a, "two.md", "# Y\n\nBody of Y.\n").await;
        full_sync(&a, &b).await;
        let x = uuid_at(&b, "one.md");
        let y = uuid_at(&b, "two.md");
        assert_ne!(x, y, "precondition: two distinct documents");

        // A swaps them: X one.md→two.md and Y two.md→one.md. A direct swap can't move
        // into an occupied path, so route Y through a temp path — the net tree effect is
        // a clean two-node swap, which is what B's import sees.
        move_file(&a, &fs_a, "two.md", "_swap_tmp.md").await; // Y: two.md -> tmp
        move_file(&a, &fs_a, "one.md", "two.md").await; // X: one.md -> two.md
        move_file(&a, &fs_a, "_swap_tmp.md", "one.md").await; // Y: tmp  -> one.md

        // B imports the swap. The swap is zero-content (INV-1), so no document content
        // crosses — each moved `.md` is re-materialized on B from its local `<uuid>.loro`,
        // and the swap branch must keep each re-occupied old path's `.md`.
        sync_both_ways(&a, &b).await;

        // Both documents survive on B at their swapped paths, with the right UUID and
        // body each — neither file was deleted by the other move's cleanup.
        assert_eq!(uuid_at(&b, "two.md"), x, "X now lives at two.md on B");
        assert_eq!(uuid_at(&b, "one.md"), y, "Y now lives at one.md on B");
        assert!(
            read_md_str(&fs_b, "two.md").await.contains("Body of X."),
            "X's body is at two.md on B (not deleted by Y's move cleanup)"
        );
        assert!(
            read_md_str(&fs_b, "one.md").await.contains("Body of Y."),
            "Y's body is at one.md on B (not deleted by X's move cleanup)"
        );

        // Exactly the two swapped files, nothing stray (no leaked temp path, no
        // conflict files — a swap is not a collision).
        assert_eq!(
            md_files(&b).await,
            BTreeSet::from(["one.md".to_string(), "two.md".to_string()]),
            "exactly the two swapped files survive on B"
        );
    }
}
