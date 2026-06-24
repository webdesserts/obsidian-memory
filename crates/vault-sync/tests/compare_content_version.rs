//! Phase-3 COMPARE protocol acceptance tests — content-version locality + FR-6 compare.
//!
//! Drives the public `Vault` API over the in-memory `Vec<u8>` seam — no real
//! on-disk vault, no iroh. Mirrors `tests/sync.rs`: `mod common; use common::*;`.
//!
//! Covers `content_version` as a per-replica-LOCAL value (the root-cause fix: a peer's
//! Index delta can't overwrite the fingerprint backing the digest), plus
//! `compare(theirs) → ChangeManifest` — the side-effect-free §6.1 classification of
//! what differs, by UUID, computed from version vectors alone.

use vault_sync::{DocComparison, StructuralComparison, SyncMessage};

mod common;
use common::*;

/// Resolution #2 — `content_version` is a per-replica-LOCAL value, structurally
/// immune to a peer's Index delta. This is the root-cause fix the whole effort exists
/// for: the fingerprint backing the digest can no longer be overwritten by a peer's
/// value riding the Index snapshot ahead of the content it fingerprints, so the
/// convergence trap (digest falsely matches → no pull) is structurally impossible.
mod content_version_is_local {
    use super::*;
    use std::collections::HashMap;

    /// Applying a peer's Index delta does NOT change THIS replica's `content_version`
    /// for a document whose CONTENT it lacks — the value is local-only and unsyncable.
    ///
    /// A and B converge on `shared.md`, then B edits it (B's local fingerprint for that
    /// doc advances). A then imports ONLY B's Index — its full snapshot, carrying B's
    /// bumped `content_version` meta for `shared.md` (in C1; nothing relevant in C2) —
    /// with the document content WITHHELD (the exclude path: a node's meta ships without
    /// its body). The assertion isolates the local-immunity property: A's
    /// `node_content_version` for `shared.md` is UNCHANGED from before the import,
    /// because the digest now reads A's LOCAL table, not the merged Index meta.
    ///
    /// This FAILS on the pre-fix (meta-reading) digest: B's meta LWW-wins on import, so
    /// A's `node_content_version` would flip to B's value — the lie the digest then
    /// trusted. It PASSES here because the value lives in a map no peer can write. (The
    /// whole-digest is NOT asserted unchanged: importing B's structural ops legitimately
    /// advances A's Index VV, which the digest folds in — only the per-UUID content
    /// fingerprint is the local-immune quantity.)
    #[tokio::test]
    async fn peer_index_delta_does_not_change_our_local_content_version() {
        let (a, b, fs_a, fs_b) = two_vaults().await;

        // Converge both replicas on a shared document.
        write_and_index(&a, &fs_a, "shared.md", "base shared\n").await;
        sync_both_ways(&a, &b).await;
        assert_converged(&a, &fs_a, &b, &fs_b).await;

        // A's own local fingerprint for shared.md (its truth, derived from A's content).
        let a_node = a.index().node_for_path("shared.md").unwrap();
        let a_fingerprint_before = a
            .index()
            .node_content_version(&a_node)
            .expect("A holds a content_version for the converged shared.md");

        // B edits shared.md offline: B's local fingerprint for it advances away from A's.
        write_and_index(&b, &fs_b, "shared.md", "base shared\nB's line\n").await;
        let b_node = b.index().node_for_path("shared.md").unwrap();
        let b_fingerprint = b.index().node_content_version(&b_node).unwrap();
        assert_ne!(
            a_fingerprint_before, b_fingerprint,
            "precondition: B's edit moved B's fingerprint away from A's"
        );

        // A imports ONLY B's Index (the full snapshot carries B's bumped content_version
        // meta for shared.md) with the document content WITHHELD — the exclude path in
        // isolation: a node's meta merges in, its body does not.
        let b_index_snapshot = b.index().export_snapshot().unwrap();
        let exclude_content_response = SyncMessage::SyncResponse {
            index_updates: Some(b_index_snapshot),
            document_updates: HashMap::new(),
        };
        let wire = bincode::serialize(&exclude_content_response).unwrap();
        a.process_message(&wire).await.unwrap();

        // A's local fingerprint for shared.md is UNCHANGED — the merged Index meta
        // (B's value, in C1) did NOT reach the local table the digest reads. A still
        // lacks B's content, and its content_version honestly reflects that.
        let a_node_after = a.index().node_for_path("shared.md").unwrap();
        assert_eq!(
            a.index().node_content_version(&a_node_after),
            Some(a_fingerprint_before),
            "a peer's Index delta must not change our local content_version for a doc \
             whose content we lack (the value is local-only, immune to the merged meta)"
        );
    }
}

/// AC-§6 (Prune-1 / FR-6): `compare(theirs) → ChangeManifest` — the side-effect-free
/// classification of what differs between us and a peer, computed from version vectors
/// alone. The classification orientation (`WeAhead` vs `TheyAhead`) is the
/// correctness-critical surface: a flip would flow deltas the wrong way. These tests
/// pin each ladder arm against a real divergence, pin the orientation at the
/// full-vault level via a swap-sides symmetry check, and prove that reconciling
/// exactly the non-`Identical` entries converges (the FR-6 completeness guarantee).
mod ac_fr6_compare {
    use super::*;

    /// AC-§6 first half: two fully-synced replicas → an all-`Identical` manifest.
    /// Identical entries are omitted (OQ-B1), so `documents` is empty AND the
    /// structure classifies `Identical`. This is the "no false non-identical when
    /// truly equal" guarantee — here the histories ARE shared, so the VVs ARE equal.
    #[tokio::test]
    async fn all_identical_when_converged() {
        let (a, b, fs_a, fs_b) = two_vaults().await;

        write_and_index(&a, &fs_a, "notes/alpha.md", "# Alpha\n\nbody\n").await;
        write_and_index(&a, &fs_a, "notes/beta.md", "# Beta\n\nbody\n").await;
        write_and_index(&b, &fs_b, "notes/gamma.md", "# Gamma\n\nbody\n").await;
        sync_both_ways(&a, &b).await;
        assert_converged(&a, &fs_a, &b, &fs_b).await;

        let manifest = a.compare(&request_data(&b).await).await.unwrap();

        assert!(
            manifest.documents.is_empty(),
            "a converged pair yields no non-Identical document entries, got {:?}",
            manifest.documents
        );
        assert_eq!(
            manifest.structural,
            StructuralComparison::Identical,
            "a converged pair has identical catalog structure"
        );
    }

    /// AC-§6 second half: ONE changed document in a 100-doc vault → exactly one
    /// non-`Identical` entry (the rest omitted), and ZERO content transferred to
    /// produce the manifest. "Zero content" is structural here: a `ChangeManifest`
    /// carries only classifications (`DocComparison`), never document bytes — the
    /// compiler guarantees no content can ride in it.
    #[tokio::test]
    async fn one_changed_doc_is_the_only_non_identical() {
        let (a, b, fs_a, fs_b) = two_vaults().await;

        // A 100-document vault, fully converged.
        for i in 0..100 {
            let path = format!("notes/doc-{i:03}.md");
            write_and_index(&a, &fs_a, &path, &format!("# Doc {i}\n\nbody {i}\n")).await;
        }
        sync_both_ways(&a, &b).await;
        assert_converged(&a, &fs_a, &b, &fs_b).await;

        // Edit exactly ONE document on A.
        let edited_path = "notes/doc-042.md";
        let edited_uuid = uuid_at(&a, edited_path);
        write_and_index(
            &a,
            &fs_a,
            edited_path,
            "# Doc 42\n\nEDITED body — diverged\n",
        )
        .await;

        let manifest = a.compare(&request_data(&b).await).await.unwrap();

        assert_eq!(
            manifest.documents.len(),
            1,
            "exactly one document diverged in a 100-doc vault, got {:?}",
            manifest.documents
        );
        assert_eq!(
            manifest.documents.get(&edited_uuid.into()).copied(),
            Some(DocComparison::WeAhead),
            "the one edited doc is WeAhead (A holds the edit B lacks)"
        );
    }

    /// A UUID only WE have classifies `WeOnly`; a UUID only THEY have classifies
    /// `TheyOnly`. Each replica creates a document the other has never seen (no sync
    /// between), so A's compare sees its own doc as `WeOnly` and B's doc as `TheyOnly`.
    #[tokio::test]
    async fn we_only_and_they_only() {
        let (a, b, fs_a, fs_b) = two_vaults().await;

        write_and_index(&a, &fs_a, "only-on-a.md", "A's exclusive doc\n").await;
        write_and_index(&b, &fs_b, "only-on-b.md", "B's exclusive doc\n").await;
        // Deliberately NO sync — each side holds a doc the other lacks.

        let a_uuid = uuid_at(&a, "only-on-a.md");
        let b_uuid = uuid_at(&b, "only-on-b.md");

        let manifest = a.compare(&request_data(&b).await).await.unwrap();

        assert_eq!(
            manifest.documents.get(&a_uuid.into()).copied(),
            Some(DocComparison::WeOnly),
            "A's exclusive document is WeOnly"
        );
        assert_eq!(
            manifest.documents.get(&b_uuid.into()).copied(),
            Some(DocComparison::TheyOnly),
            "B's exclusive document is TheyOnly"
        );
    }

    /// A document edited offline on BOTH replicas (no sync between the edits)
    /// classifies `Concurrent` — neither side's op history includes the other's.
    #[tokio::test]
    async fn concurrent_edit_classifies_concurrent() {
        let (a, b, fs_a, fs_b) = two_vaults().await;

        write_and_index(&a, &fs_a, "shared.md", "shared base\n").await;
        sync_both_ways(&a, &b).await;
        let shared_uuid = uuid_at(&a, "shared.md");

        // Each side edits the converged doc offline; the edits never cross.
        write_and_index(&a, &fs_a, "shared.md", "shared base\nA's offline edit\n").await;
        write_and_index(&b, &fs_b, "shared.md", "shared base\nB's offline edit\n").await;

        let manifest = a.compare(&request_data(&b).await).await.unwrap();

        assert_eq!(
            manifest.documents.get(&shared_uuid.into()).copied(),
            Some(DocComparison::Concurrent),
            "a doc edited offline on both sides is Concurrent"
        );
    }

    /// The orientation pin AT THE FULL-VAULT LEVEL: the two replicas' manifests are
    /// mirror images. If A classifies a doc `WeAhead`, B (running compare with the
    /// sides swapped) classifies that SAME doc `TheyAhead`; `WeOnly` mirrors to
    /// `TheyOnly`; `Concurrent` and `Identical` are symmetric. A flipped `partial_cmp`
    /// orientation would break this mirror — and mirror manifests are the convergence
    /// prerequisite (each side ships exactly what the other is owed).
    #[tokio::test]
    async fn manifests_are_mirror_images_across_replicas() {
        let (a, b, fs_a, fs_b) = two_vaults().await;

        // A base both share, then a divergence in every direction:
        write_and_index(&a, &fs_a, "ahead.md", "base\n").await;
        write_and_index(&a, &fs_a, "concurrent.md", "base\n").await;
        sync_both_ways(&a, &b).await;

        // ahead.md: A edits it → A is ahead of B on it.
        write_and_index(&a, &fs_a, "ahead.md", "base\nA pulled ahead\n").await;
        // concurrent.md: both edit offline → concurrent.
        write_and_index(&a, &fs_a, "concurrent.md", "base\nA side\n").await;
        write_and_index(&b, &fs_b, "concurrent.md", "base\nB side\n").await;
        // only-a.md / only-b.md: exclusive to each side.
        write_and_index(&a, &fs_a, "only-a.md", "A exclusive\n").await;
        write_and_index(&b, &fs_b, "only-b.md", "B exclusive\n").await;

        let a_manifest = a.compare(&request_data(&b).await).await.unwrap();
        let b_manifest = b.compare(&request_data(&a).await).await.unwrap();

        // Both manifests cover the same UUID set (the union is symmetric).
        let a_keys: std::collections::HashSet<_> = a_manifest.documents.keys().copied().collect();
        let b_keys: std::collections::HashSet<_> = b_manifest.documents.keys().copied().collect();
        assert_eq!(
            a_keys, b_keys,
            "both replicas' manifests classify the same non-Identical UUID set"
        );

        // Every entry is the mirror of its counterpart.
        for (uuid, a_class) in &a_manifest.documents {
            let b_class = b_manifest
                .documents
                .get(uuid)
                .copied()
                .expect("mirror manifest covers the same UUID");
            assert_eq!(
                *a_class,
                mirror(b_class),
                "A's {a_class:?} for {uuid} must mirror B's {b_class:?}"
            );
        }

        // Structural classification is likewise mirrored.
        assert_eq!(
            a_manifest.structural,
            mirror_structural(b_manifest.structural),
            "the structural classification mirrors across replicas"
        );
    }

    /// The mirror of a [`DocComparison`] when the two replicas swap roles: A's view of
    /// a doc is the role-swap of B's view of the same doc.
    fn mirror(c: DocComparison) -> DocComparison {
        match c {
            DocComparison::Identical => DocComparison::Identical,
            DocComparison::Concurrent => DocComparison::Concurrent,
            DocComparison::WeAhead => DocComparison::TheyAhead,
            DocComparison::TheyAhead => DocComparison::WeAhead,
            DocComparison::WeOnly => DocComparison::TheyOnly,
            DocComparison::TheyOnly => DocComparison::WeOnly,
        }
    }

    /// The mirror of a [`StructuralComparison`] under a role swap.
    fn mirror_structural(c: StructuralComparison) -> StructuralComparison {
        match c {
            StructuralComparison::Identical => StructuralComparison::Identical,
            StructuralComparison::Concurrent => StructuralComparison::Concurrent,
            StructuralComparison::WeAhead => StructuralComparison::TheyAhead,
            StructuralComparison::TheyAhead => StructuralComparison::WeAhead,
        }
    }

    /// The FR-6 completeness guarantee (§6.1 third bullet): reconciling exactly the
    /// non-`Identical` manifest entries — in the direction each indicates — is
    /// NECESSARY AND SUFFICIENT for convergence. Build a mixed divergence covering
    /// every ahead/behind/concurrent/only direction, snapshot the manifest, then run a
    /// real `full_sync` both ways and assert the replicas converge — proving the
    /// manifest named exactly the work that needed doing. Bridges Chunk B's manifest to
    /// Chunk D's convergence proof.
    #[tokio::test]
    async fn completeness_reconciling_the_manifest_converges() {
        let (a, b, fs_a, fs_b) = two_vaults().await;

        // A shared base for the ahead + concurrent docs.
        write_and_index(&a, &fs_a, "we-ahead.md", "base\n").await;
        write_and_index(&a, &fs_a, "they-ahead.md", "base\n").await;
        write_and_index(&a, &fs_a, "concurrent.md", "base\n").await;
        sync_both_ways(&a, &b).await;

        // WeAhead: A edits.
        write_and_index(&a, &fs_a, "we-ahead.md", "base\nA ahead\n").await;
        // TheyAhead: B edits.
        write_and_index(&b, &fs_b, "they-ahead.md", "base\nB ahead\n").await;
        // Concurrent: both edit offline.
        write_and_index(&a, &fs_a, "concurrent.md", "base\nA side\n").await;
        write_and_index(&b, &fs_b, "concurrent.md", "base\nB side\n").await;
        // WeOnly / TheyOnly: exclusive new docs.
        write_and_index(&a, &fs_a, "we-only.md", "A exclusive\n").await;
        write_and_index(&b, &fs_b, "they-only.md", "B exclusive\n").await;

        // The manifest names every direction of divergence (and nothing converged).
        let manifest = a.compare(&request_data(&b).await).await.unwrap();
        let classes: std::collections::HashSet<DocComparison> =
            manifest.documents.values().copied().collect();
        assert!(
            classes.contains(&DocComparison::WeAhead)
                && classes.contains(&DocComparison::TheyAhead)
                && classes.contains(&DocComparison::Concurrent)
                && classes.contains(&DocComparison::WeOnly)
                && classes.contains(&DocComparison::TheyOnly),
            "the manifest covers every divergence direction, got {:?}",
            manifest.documents
        );

        // Reconciling exactly those entries (a real sync both ways) converges.
        sync_both_ways(&a, &b).await;
        assert_converged(&a, &fs_a, &b, &fs_b).await;

        // And after convergence, the manifest is all-Identical (empty) — the
        // ack-by-convergence the Issue-2 layer relies on.
        let after = a.compare(&request_data(&b).await).await.unwrap();
        assert!(
            after.documents.is_empty(),
            "after reconciling, the manifest is all-Identical, got {:?}",
            after.documents
        );
    }

    /// OQ-B2: an undecodable peer VV for a UUID we ALSO hold classifies `Concurrent`
    /// (the safe over-approximation — it forces a converging delta exchange, never a
    /// silent drop) and never panics. Built by taking a real converged peer summary and
    /// corrupting the one shared document's VV bytes.
    #[tokio::test]
    async fn undecodable_peer_vv_classifies_concurrent() {
        let (a, b, fs_a, _fs_b) = two_vaults().await;

        write_and_index(&a, &fs_a, "shared.md", "shared body\n").await;
        sync_both_ways(&a, &b).await;
        let shared_uuid = uuid_at(&a, "shared.md");

        // Take B's real summary, then corrupt the VV bytes for the shared doc so it
        // fails to decode — a corrupt-summary input the compare must tolerate.
        let mut summary = request_data(&b).await;
        summary.document_versions.insert(
            shared_uuid.into(),
            b"\xff\xff not a version vector".to_vec(),
        );

        let manifest = a.compare(&summary).await.unwrap();

        assert_eq!(
            manifest.documents.get(&shared_uuid.into()).copied(),
            Some(DocComparison::Concurrent),
            "an undecodable peer VV for a doc we hold classifies Concurrent (safe over-approximation)"
        );
    }
}
