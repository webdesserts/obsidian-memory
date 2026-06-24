//! Phase-3 COMPARE protocol acceptance tests — the capstone (delta isolation + N-replica).
//!
//! Drives the public `Vault` API over the in-memory `Vec<u8>` seam — no real
//! on-disk vault, no iroh. Mirrors `tests/sync.rs`: `mod common; use common::*;`.
//!
//! The whole compare protocol end-to-end against the §9 ACs: AC-§6 half-2 (one edit in
//! a 100-doc vault transfers only that doc's delta, strictly smaller than its
//! snapshot), N-replica manifest completeness, manifest correctness under the
//! lossy-transport contract, and `compare`'s side-effect-freedom.

use vault_sync::{DocComparison, StructuralComparison};

mod common;
use common::*;

/// A deterministic, NON-repetitive document body sized so a loro snapshot is
/// MEANINGFULLY larger than a one-line-edit delta — the magnitude precondition the
/// AC-§6 half-2 proof rests on.
///
/// Two properties matter for a faithful (non-vacuous) proof:
/// 1. **Non-trivial size** — 50 lines × 12 words ≈ 11 KB of markdown, so a snapshot
///    is far larger than a tiny edit's incremental delta. A tiny body would make
///    snapshot ≈ delta and the `delta < snapshot` assertion would pass vacuously.
/// 2. **Non-repetitive content** — each word is drawn from a SplitMix64 stream keyed
///    by the document seed, so loro's snapshot text-compression can't collapse the
///    body to near-nothing. (With repetitive filler text loro compresses the
///    snapshot heavily, shrinking the very gap this proof needs; varied content keeps
///    the gap honest — measured ~22 KB snapshot vs ~113 B delta, a ~200× margin.)
fn varied_body(seed: usize) -> String {
    let mut x = (seed as u64)
        .wrapping_mul(0x9E37_79B9_7F4A_7C15)
        .wrapping_add(1);
    let mut next_word = || {
        x = x.wrapping_add(0x9E37_79B9_7F4A_7C15);
        let mut z = x;
        z = (z ^ (z >> 30)).wrapping_mul(0xBF58_476D_1CE4_E5B9);
        z = (z ^ (z >> 27)).wrapping_mul(0x94D0_49BB_1331_11EB);
        z ^ (z >> 31)
    };
    let mut s = format!("# Doc {seed}\n\n");
    for line in 0..50 {
        s.push_str(&format!("L{line}:"));
        for _ in 0..12 {
            s.push_str(&format!(" w{:x}", next_word()));
        }
        s.push('\n');
    }
    s
}

/// AC-§6 (the §6.2 "cost scales with what changed" half), made concrete: after a
/// single edit in a large vault, the compare layer isolates exactly that one document
/// AND the subsequent sync ships ONLY that document's incremental delta — strictly
/// smaller than a full snapshot, nowhere near re-shipping the vault.
///
/// This is the single most important P3 test: it proves the whole "transfer only the
/// delta" thesis. The faithfulness hinges on the magnitude gap being real, not
/// coincidental — see [`varied_body`] for why the bodies are large and non-repetitive,
/// and note the assertions capture both magnitudes (snapshot length and transferred
/// bytes) as named values so the proof's margin is visible.
mod ac_6_delta_isolation {
    use super::*;

    /// Converge a 100-document vault (each doc a non-trivial ~11 KB body), edit ONE
    /// doc on A with a small one-line append, then `full_sync` under a `ByteCounter`.
    /// Assert: (a) total document-content transferred is on the order of one small
    /// delta — well below a single snapshot, light-years below 100 snapshots; (b) that
    /// total is STRICTLY LESS than the edited doc's `export_snapshot()` size (both
    /// magnitudes captured explicitly); (c) the pre-sync `compare` names exactly ONE
    /// non-`Identical` entry — the edited doc, `WeAhead`.
    #[tokio::test]
    async fn one_changed_doc_transfers_only_its_delta_smaller_than_snapshot() {
        let (a, b, fs_a, fs_b) = two_vaults().await;

        // A 100-document vault, each with a meaningfully-large non-repetitive body,
        // fully converged so B holds every document's full history.
        for i in 0..100 {
            let path = format!("notes/doc-{i:03}.md");
            write_and_index(&a, &fs_a, &path, &varied_body(i)).await;
        }
        sync_both_ways(&a, &b).await;
        assert_converged(&a, &fs_a, &b, &fs_b).await;

        // Edit exactly ONE document on A with a SMALL change (append one short line).
        let edited_path = "notes/doc-042.md";
        let edited_uuid = uuid_at(&a, edited_path);
        let mut edited = varied_body(42);
        edited.push_str("One small appended line.\n");
        write_and_index(&a, &fs_a, edited_path, &edited).await;

        // Capture the edited doc's FULL snapshot size — the magnitude a naive
        // "re-ship the whole doc" sync would transfer. The delta must beat this.
        let snapshot_len = a
            .get_document(edited_path)
            .await
            .unwrap()
            .export_snapshot()
            .unwrap()
            .len();

        // (c) The compare layer isolates exactly the one changed document BEFORE any
        // content moves — exactly one non-`Identical` entry, classified `WeAhead`.
        let manifest = a.compare(&request_data(&b).await).await.unwrap();
        assert_eq!(
            manifest.documents.len(),
            1,
            "exactly one doc diverged in a 100-doc vault, got {:?}",
            manifest.documents
        );
        assert_eq!(
            manifest.documents.get(&edited_uuid.into()).copied(),
            Some(DocComparison::WeAhead),
            "the one edited doc is WeAhead (A holds the edit B lacks)"
        );

        // Run the actual sync under a byte counter and measure what content crossed.
        let mut counter = ByteCounter::new();
        let (_modified_a, modified_b) = counter.full_sync_counting(&a, &b).await;
        let transferred = counter.total_document_content_bytes();

        // The sync carried exactly the one edited document to B (and nothing back).
        assert_eq!(
            modified_b,
            vec![edited_uuid.into()],
            "the sync delivered exactly the one edited doc to B, got {modified_b:?}"
        );

        // (b) THE headline assertion: the transferred content is strictly smaller than
        // a single full snapshot of the edited doc — captured as named magnitudes so
        // the margin is unmistakable (measured ≈113 B transferred vs ≈22 KB snapshot).
        assert!(
            transferred < snapshot_len,
            "transferred content ({transferred} B) must be strictly less than the edited \
             doc's snapshot ({snapshot_len} B) — proves the sync ships a delta, not a snapshot"
        );

        // (a) And it is on the order of ONE small delta — an absolute ceiling far below
        // even one snapshot, so the proof can't pass by transferring "a snapshot but a
        // bit smaller." A one-line edit's delta is ~113 B; 2 KB is a generous ceiling
        // that is still ~10× under one snapshot and ~1000× under 100 snapshots, and
        // tolerant of loro framing/version drift.
        const SMALL_DELTA_CEILING: usize = 2_000;
        assert!(
            transferred <= SMALL_DELTA_CEILING,
            "transferred content ({transferred} B) must be on the order of one small delta \
             (≤ {SMALL_DELTA_CEILING} B), NOT a snapshot ({snapshot_len} B) and NOT ~100 \
             snapshots — the §6.2 cost-scales-with-change bound"
        );

        // Sanity floor: the sync genuinely moved the edit (a real, non-empty delta) —
        // guards against a degenerate "transferred nothing, vacuously < snapshot" pass.
        assert!(
            transferred > 0,
            "the edit's delta is non-empty — a real change crossed the wire"
        );

        // The edit actually landed on B (end-to-end, not just byte accounting).
        assert!(
            read_md_str(&fs_b, edited_path)
                .await
                .contains("One small appended line."),
            "the edited line materialized on B"
        );
        assert_converged(&a, &fs_a, &b, &fs_b).await;
    }
}

/// The §6 capstone batteries that exercise the manifest across MORE than two replicas
/// and across the lossy-transport contract, plus the purity regression guard. These
/// tie AC-§6 to AC-§5 (lossy convergence) and §5.2 (side-effect-freedom).
mod compare_capstone {
    use super::*;

    /// N-replica manifest completeness: 3 replicas with a mix of divergences (a doc
    /// only on A, a doc only on B, a concurrent edit between B and C, a move on A),
    /// pumped to quiescence across all three, must converge byte-for-byte — and a
    /// `compare` between EVERY pair after convergence must be all-`Identical` (empty
    /// manifests, identical structure). The N-replica generalization of the §6.1
    /// completeness guarantee: the manifest names exactly the work, and reconciling it
    /// across a mesh leaves nothing left to classify.
    #[tokio::test]
    async fn n_replica_manifest_completeness() {
        let replicas = n_vaults(3).await;
        let (a, fs_a) = (&replicas[0].0, &replicas[0].1);
        let (b, fs_b) = (&replicas[1].0, &replicas[1].1);
        let (c, fs_c) = (&replicas[2].0, &replicas[2].1);

        // A shared base for the concurrent edit + the move, converged across the mesh
        // first so the later divergences are genuinely concurrent.
        write_and_index(a, fs_a, "shared.md", "base shared\n").await;
        write_and_index(a, fs_a, "old/moved.md", "stable content\n").await;
        let refs: Vec<&V> = replicas.iter().map(|(v, _)| v).collect();
        pump_to_quiescence(&refs).await;

        // A mix of divergences across the three replicas:
        // - a doc only on A (WeOnly from A's view)
        write_and_index(a, fs_a, "only-on-a.md", "A exclusive\n").await;
        // - a doc only on B
        write_and_index(b, fs_b, "only-on-b.md", "B exclusive\n").await;
        // - a concurrent edit to shared.md between B and C (each edits offline)
        write_and_index(b, fs_b, "shared.md", "base shared\nB's line\n").await;
        write_and_index(c, fs_c, "shared.md", "base shared\nC's line\n").await;
        // - a pure move on A
        move_file(a, fs_a, "old/moved.md", "new/moved.md").await;

        // Pump the whole mesh to quiescence and assert byte-identical convergence.
        pump_to_quiescence(&refs).await;
        let pairs: Vec<(&V, &Fs)> = replicas.iter().map(|(v, fs)| (v, fs)).collect();
        assert_all_converged(&pairs).await;

        // After convergence, EVERY ordered pair's manifest is all-`Identical` — empty
        // document set AND identical structure. Nothing left for the manifest to name.
        for (i, (vi, _)) in replicas.iter().enumerate() {
            for (j, (vj, _)) in replicas.iter().enumerate() {
                if i == j {
                    continue;
                }
                let manifest = vi.compare(&request_data(vj).await).await.unwrap();
                assert!(
                    manifest.documents.is_empty(),
                    "after convergence, replica {i} vs {j} has no non-Identical docs, got {:?}",
                    manifest.documents
                );
                assert_eq!(
                    manifest.structural,
                    StructuralComparison::Identical,
                    "after convergence, replica {i} vs {j} has identical structure"
                );
            }
        }

        // The concurrent edit merged both lines everywhere (the divergence really was
        // reconciled, not silently dropped).
        let merged = read_md_str(fs_a, "shared.md").await;
        assert!(
            merged.contains("B's line") && merged.contains("C's line"),
            "shared.md merged both concurrent edits across the mesh: {merged:?}"
        );
    }

    /// `compare` + sync stays correct under the §5 lossy-transport contract: build a
    /// mixed divergence across 3 replicas, converge over a hostile channel (random
    /// drop / duplicate / reorder via `pump_lossy_to_quiescence`), and assert the
    /// replicas converge AND a final `compare` between each pair is all-`Identical`.
    /// Ties AC-§6 (the manifest is complete) to AC-§5 (the lib tolerates an unreliable
    /// transport because every exchange re-derives what differs from version vectors).
    #[tokio::test]
    async fn compare_then_sync_is_idempotent_under_lossy_transport() {
        let replicas = n_vaults(3).await;
        let (a, fs_a) = (&replicas[0].0, &replicas[0].1);
        let (b, fs_b) = (&replicas[1].0, &replicas[1].1);
        let (c, fs_c) = (&replicas[2].0, &replicas[2].1);

        // A shared base for the concurrent edit, converged first.
        write_and_index(a, fs_a, "shared.md", "base\n").await;
        let refs: Vec<&V> = replicas.iter().map(|(v, _)| v).collect();
        pump_to_quiescence(&refs).await;

        // Mixed divergence: an exclusive doc on each side + a three-way concurrent edit
        // to the shared doc (each replica edits it offline).
        write_and_index(a, fs_a, "from-a.md", "A exclusive\n").await;
        write_and_index(b, fs_b, "from-b.md", "B exclusive\n").await;
        write_and_index(c, fs_c, "from-c.md", "C exclusive\n").await;
        write_and_index(a, fs_a, "shared.md", "base\nA edit\n").await;
        write_and_index(b, fs_b, "shared.md", "base\nB edit\n").await;
        write_and_index(c, fs_c, "shared.md", "base\nC edit\n").await;

        // Converge over the hostile (drop/dup/reorder) channel. The seed makes the loss
        // pattern reproducible — a failure here is a real non-convergence bug.
        let iterations =
            pump_lossy_to_quiescence(&refs, LossProfile::hostile(), 0xC0FF_EED0_0D0D).await;
        assert!(
            (1..=50).contains(&iterations),
            "lossy convergence should settle quickly (took {iterations} iterations)"
        );

        let pairs: Vec<(&V, &Fs)> = replicas.iter().map(|(v, fs)| (v, fs)).collect();
        assert_all_converged(&pairs).await;

        // A final compare between each pair is all-`Identical`: the manifest stays
        // correct after the lossy reconciliation — no divergence the lossy channel
        // smuggled past it.
        for (i, (vi, _)) in replicas.iter().enumerate() {
            for (j, (vj, _)) in replicas.iter().enumerate() {
                if i == j {
                    continue;
                }
                let manifest = vi.compare(&request_data(vj).await).await.unwrap();
                assert!(
                    manifest.documents.is_empty(),
                    "post-lossy-convergence, replica {i} vs {j} manifest is all-Identical, got {:?}",
                    manifest.documents
                );
            }
        }
    }

    /// The §5.2 purity regression guard: `compare` is SIDE-EFFECT-FREE. Capture a
    /// vault's `catalog_digest()` AND the full set of materialized `.md` paths and
    /// contents, call `compare` against SEVERAL different peer summaries, then assert
    /// the digest and ALL materialized `.md` state are byte-unchanged afterward.
    /// Compare reads version vectors and classifies; it must never touch the catalog or
    /// materialize/move/delete a file as a side effect.
    #[tokio::test]
    async fn compare_is_side_effect_free() {
        let (a, b, fs_a, fs_b) = two_vaults().await;
        // A third peer summary with a different divergence shape, so `compare` is
        // exercised against several distinct inputs (in-sync, ahead, only, concurrent).
        let (c, fs_c) = one_vault().await;

        // A's vault: a converged doc, a doc only A has, and a doc concurrently edited.
        write_and_index(&a, &fs_a, "shared.md", "shared base\n").await;
        write_and_index(&a, &fs_a, "only-a.md", "A exclusive\n").await;
        write_and_index(&a, &fs_a, "concurrent.md", "base\n").await;
        sync_both_ways(&a, &b).await;
        sync_both_ways(&a, &c).await;

        // Diverge against the peers in different ways.
        write_and_index(&a, &fs_a, "concurrent.md", "base\nA side\n").await;
        write_and_index(&b, &fs_b, "concurrent.md", "base\nB side\n").await;
        write_and_index(&b, &fs_b, "only-b.md", "B exclusive\n").await;
        write_and_index(&c, &fs_c, "only-c.md", "C exclusive\n").await;

        // Snapshot A's full observable state BEFORE any compare.
        let digest_before = a.catalog_digest();
        let paths_before = alive_md_paths(&a).await;
        let mut contents_before: Vec<(String, String)> = Vec::new();
        for path in &paths_before {
            contents_before.push((path.clone(), read_md_str(&fs_a, path).await));
        }

        // Call compare against several distinct peer summaries.
        let summaries = [
            request_data(&b).await,
            request_data(&c).await,
            request_data(&a).await, // self-summary: an all-Identical input
        ];
        for summary in &summaries {
            let _ = a.compare(summary).await.unwrap();
        }

        // A's digest is byte-unchanged — compare did not touch the catalog.
        assert_eq!(
            a.catalog_digest(),
            digest_before,
            "compare must not change the catalog digest (side-effect-free)"
        );

        // A's materialized .md set and every file's content are byte-unchanged —
        // compare materialized/moved/deleted nothing.
        let paths_after = alive_md_paths(&a).await;
        assert_eq!(
            paths_after, paths_before,
            "compare must not add/remove any materialized .md file"
        );
        for (path, content_before) in &contents_before {
            assert_eq!(
                &read_md_str(&fs_a, path).await,
                content_before,
                "compare must not modify the content of {path}"
            );
        }
    }
}
