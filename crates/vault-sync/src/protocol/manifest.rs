//! The compare protocol's change-manifest (§6.1): a side-effect-free classification
//! of what differs between this replica and a peer, computed from version vectors
//! alone — zero content crosses the wire to produce it.
//!
//! This is the Issue-2 enabler: the later reliable-delivery layer pulls a manifest,
//! persists "peer X is owed documents [A, B, C]", ships exactly those deltas, and
//! confirms convergence by a subsequent manifest showing everything `Identical`
//! (ack-by-convergence). The library provides the manifest; it owns no delivery
//! durability (§6.3).
//!
//! ## The classification is exact at the CRDT-state level (§6.1, N3)
//!
//! Two replicas' copies of a document are `Identical` iff they have applied the SAME
//! set of operations (equal version vectors), which — given INV-4's deterministic
//! serialization — implies byte-identical materialized markdown. The ahead/behind
//! classes hold iff one VV strictly includes the other; `Concurrent` iff neither
//! includes the other (both diverged). There is **no false `Identical`**: equal VVs
//! cannot yield different materialized content. The converse is not guaranteed — two
//! copies that materialize to the same markdown via different op histories carry
//! different VVs and classify `Concurrent` (a **spurious-`Concurrent`**: it triggers
//! a redundant delta exchange that converges to a no-op, never data loss). That is a
//! bounded completeness imperfection, not a correctness failure.

use std::collections::HashMap;

use loro::VersionVector;

use super::{DocId, SyncRequestData};

/// How one document's version on this replica relates to a peer's (§6.1 ladder).
/// Computed purely from version vectors — NO content is read to produce it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DocComparison {
    /// Equal VVs — same applied op set ⇒ byte-identical materialized markdown (INV-4).
    Identical,
    /// We strictly include theirs — we owe them a delta (`export_updates(from: theirs)`).
    WeAhead,
    /// They strictly include ours — they owe us a delta (we request it).
    TheyAhead,
    /// Neither includes the other — both diverged; both exchange deltas + merge (cascade).
    Concurrent,
    /// We have this UUID, they don't — we ship the doc + its node (WeOnly).
    WeOnly,
    /// They have this UUID, we don't — we request it (TheyOnly).
    TheyOnly,
}

/// Whether the structure (folder tree / catalog) differs, from the two Index VVs.
///
/// No `WeOnly`/`TheyOnly` — both replicas always have an Index, so the only outcomes
/// are the four inclusion relations. Per §6.1, two replicas with the same
/// materialized tree but different concurrent-op subsets classify `Concurrent` and
/// re-exchange (the accepted spurious-concurrent: a redundant transfer, never a
/// divergence).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StructuralComparison {
    /// Equal Index VVs — identical merged catalog (up to spurious-concurrent, §6.1).
    Identical,
    /// Our Index VV strictly includes theirs — we owe them structural updates.
    WeAhead,
    /// Their Index VV strictly includes ours — they owe us structural updates.
    TheyAhead,
    /// Neither Index VV includes the other — both structures diverged.
    Concurrent,
}

/// The §6.1 change-manifest: what differs between us and a peer, by UUID, no content.
///
/// `documents` keys are document UUIDs; the value is each document's classification.
/// `Identical` entries are OMITTED (OQ-B1) so the manifest's size scales with the
/// CHANGED set, not the vault: a 5000-document vault with one edit yields a one-entry
/// manifest. A consumer that wants the full census can diff against its own known set.
#[derive(Debug, Clone)]
pub struct ChangeManifest {
    /// How our catalog structure relates to the peer's (from the two Index VVs).
    pub structural: StructuralComparison,
    /// Every UUID whose classification is NOT `Identical`, keyed by document UUID.
    /// Reconciling exactly these entries (in the direction each indicates) is
    /// necessary and sufficient for convergence (§6.1, INV-4).
    pub documents: HashMap<DocId, DocComparison>,
}

/// Classify one document from the two replicas' version vectors (the load-bearing
/// pure ladder — §6.1 exact). `None` means the UUID is absent on that side.
///
/// Orientation (empirically confirmed against loro's `VersionVector::partial_cmp`):
/// when OURS strictly includes THEIRS, `ours.partial_cmp(theirs) == Some(Greater)`,
/// so `Some(Greater)` maps to `WeAhead` (we hold ops they lack ⇒ WE send). Reversing
/// this is the silent-divergence trap the unit orientation tests pin independently.
///
/// `(None, None)` is unreachable from `compare` (it iterates OURS ∪ THEIRS, so every
/// UUID is present on at least one side) but is classified `Concurrent` as the safe
/// over-approximation rather than panicking.
fn classify(ours: Option<&VersionVector>, theirs: Option<&VersionVector>) -> DocComparison {
    use std::cmp::Ordering;
    match (ours, theirs) {
        (Some(ours), Some(theirs)) => match ours.partial_cmp(theirs) {
            Some(Ordering::Equal) => DocComparison::Identical,
            Some(Ordering::Greater) => DocComparison::WeAhead,
            Some(Ordering::Less) => DocComparison::TheyAhead,
            None => DocComparison::Concurrent,
        },
        (Some(_), None) => DocComparison::WeOnly,
        (None, Some(_)) => DocComparison::TheyOnly,
        (None, None) => DocComparison::Concurrent,
    }
}

/// Classify two Index VVs into a [`StructuralComparison`] (the same four-way ladder
/// as a document, minus the only-on-one-side cases — both replicas always have an
/// Index). An Index VV that fails to decode is treated as `Concurrent` (the safe
/// over-approximation: forces a structural re-exchange that converges to a no-op).
fn classify_structural(ours: &VersionVector, their_bytes: &[u8]) -> StructuralComparison {
    use std::cmp::Ordering;
    let Ok(theirs) = VersionVector::decode(their_bytes) else {
        return StructuralComparison::Concurrent;
    };
    match ours.partial_cmp(&theirs) {
        Some(Ordering::Equal) => StructuralComparison::Identical,
        Some(Ordering::Greater) => StructuralComparison::WeAhead,
        Some(Ordering::Less) => StructuralComparison::TheyAhead,
        None => StructuralComparison::Concurrent,
    }
}

use crate::fs::FileSystem;
use crate::vault::Vault;

use super::Result;

impl<F: FileSystem> Vault<F> {
    /// Classify what differs between us and a peer, from the peer's version summary
    /// (its Index VV + per-document VVs). The §6.1 change-manifest / FR-6.
    ///
    /// **Side-effect-free.** It reads our Index VV and each of our live documents'
    /// authoritative `version()`, decodes the peer's VVs, and classifies the UNION of
    /// UUIDs known to either side. It mutates nothing, materializes nothing, and never
    /// touches the apply path — the reviewer can verify there is no filesystem or Index
    /// mutation anywhere in this call tree. (It opens content docs to read their
    /// `version()`; that is a read, matching how `prepare_request_data` already reads
    /// VVs — OQ-B3's lean: read the authoritative `version()` for correctness, since
    /// `compare` is the EXACT what-differs surface, not the no-op hot path.)
    ///
    /// `async` because building our side enumerates our live documents (`list_files` +
    /// `get_document(...).version()`, the same loop `prepare_request_data` runs). This
    /// is NOT the O(1) no-op hot path — that is the digest (§6.2). `compare` is the
    /// "isolate exactly what differs" surface and runs only on a digest miss.
    ///
    /// A peer VV that fails to decode for a UUID we ALSO hold is classified
    /// `Concurrent` (OQ-B2): the safe over-approximation forces a delta exchange that
    /// converges to a no-op rather than dropping the document. It never panics.
    pub async fn compare(&self, theirs: &SyncRequestData) -> Result<ChangeManifest> {
        // Our side: every live document's authoritative content VV, keyed by UUID. We
        // resolve each node's current path and open its content doc — the same read
        // `prepare_request_data` performs.
        let mut ours: HashMap<DocId, VersionVector> = HashMap::new();
        for (uuid, node_id) in self.index().live_file_nodes() {
            let Some(path) = self.index().path_for_node(&node_id) else {
                continue;
            };
            let doc = self.get_document(&path).await?;
            ours.insert(DocId(uuid), doc.version());
        }

        // Classify the UNION of UUIDs known to either side ("every document known to
        // either side," §6.1). `Identical` entries are omitted (OQ-B1).
        let mut documents: HashMap<DocId, DocComparison> = HashMap::new();
        let union: std::collections::HashSet<DocId> = ours
            .keys()
            .chain(theirs.document_versions.keys())
            .copied()
            .collect();

        for doc_id in union {
            let our_vv = ours.get(&doc_id);
            let comparison = match theirs.document_versions.get(&doc_id) {
                None => classify(our_vv, None),
                Some(their_bytes) => match VersionVector::decode(their_bytes) {
                    Ok(their_vv) => classify(our_vv, Some(&their_vv)),
                    // Undecodable peer VV (OQ-B2): if we ALSO hold it, force a
                    // converging delta exchange (`Concurrent`); if we don't, we cannot
                    // request a doc whose version we can't read, so treat it as absent.
                    Err(_) if our_vv.is_some() => DocComparison::Concurrent,
                    Err(_) => continue,
                },
            };
            if comparison != DocComparison::Identical {
                documents.insert(doc_id, comparison);
            }
        }

        let structural = classify_structural(&self.index().state_vv(), &theirs.index_version);

        Ok(ChangeManifest {
            structural,
            documents,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::content_doc::ContentDoc;

    // ===================== ladder-construction helpers =====================
    //
    // The classifier is a pure function of two version vectors, so the unit tests
    // build real `ContentDoc`s with known divergence and read their `version()` —
    // the same VVs `compare` classifies, exercised without standing up vaults.

    /// A base document snapshot two replicas can fork from the same state.
    fn base_snapshot() -> Vec<u8> {
        ContentDoc::from_markdown("base body", 0x01)
            .unwrap()
            .export_snapshot()
            .unwrap()
    }

    /// A replica forked from `base` that made one extra edit — its op set strictly
    /// includes the base's, so its VV is strictly ahead of an un-edited fork.
    fn forked_and_edited(base: &[u8], author: u64, edit: &str) -> ContentDoc {
        let doc = ContentDoc::from_bytes(base, author).unwrap();
        doc.update_body(edit).unwrap();
        doc.commit();
        doc
    }

    // ===================== the six ladder arms in isolation =====================

    #[test]
    fn equal_vvs_classify_identical() {
        let base = base_snapshot();
        let a = ContentDoc::from_bytes(&base, 0x02).unwrap();
        let b = ContentDoc::from_bytes(&base, 0x03).unwrap();
        // Same imported op set ⇒ equal VVs.
        assert_eq!(
            classify(Some(&a.version()), Some(&b.version())),
            DocComparison::Identical,
        );
    }

    /// The orientation pin: WE made an edit they lack ⇒ `WeAhead`, NOT `TheyAhead`.
    /// A flipped `partial_cmp` mapping would classify this `TheyAhead` and flow the
    /// delta the wrong way — exactly the silent-divergence trap this chunk avoids.
    #[test]
    fn ours_strictly_ahead_classifies_we_ahead() {
        let base = base_snapshot();
        let ours = forked_and_edited(&base, 0x03, "base body + our extra edit");
        let theirs = ContentDoc::from_bytes(&base, 0x02).unwrap();
        assert_eq!(
            classify(Some(&ours.version()), Some(&theirs.version())),
            DocComparison::WeAhead,
        );
    }

    /// The reverse orientation pin: THEY made an edit we lack ⇒ `TheyAhead`.
    #[test]
    fn theirs_strictly_ahead_classifies_they_ahead() {
        let base = base_snapshot();
        let ours = ContentDoc::from_bytes(&base, 0x02).unwrap();
        let theirs = forked_and_edited(&base, 0x03, "base body + their extra edit");
        assert_eq!(
            classify(Some(&ours.version()), Some(&theirs.version())),
            DocComparison::TheyAhead,
        );
    }

    #[test]
    fn divergent_offline_edits_classify_concurrent() {
        let base = base_snapshot();
        // Each replica forks from the same base and edits independently — neither's
        // op set includes the other's.
        let ours = forked_and_edited(&base, 0x04, "edit on our side only");
        let theirs = forked_and_edited(&base, 0x05, "edit on their side only");
        assert_eq!(
            classify(Some(&ours.version()), Some(&theirs.version())),
            DocComparison::Concurrent,
        );
    }

    #[test]
    fn ours_only_classifies_we_only() {
        let ours = ContentDoc::from_markdown("only we have this", 0x01).unwrap();
        assert_eq!(classify(Some(&ours.version()), None), DocComparison::WeOnly);
    }

    #[test]
    fn theirs_only_classifies_they_only() {
        let theirs = ContentDoc::from_markdown("only they have this", 0x01).unwrap();
        assert_eq!(
            classify(None, Some(&theirs.version())),
            DocComparison::TheyOnly,
        );
    }

    /// `(None, None)` is unreachable from `compare` (it iterates the union), but the
    /// classifier must not panic on it — the safe over-approximation is `Concurrent`.
    #[test]
    fn neither_side_classifies_concurrent_without_panicking() {
        assert_eq!(classify(None, None), DocComparison::Concurrent);
    }

    // ===================== structural classification =====================

    #[test]
    fn structural_equal_index_vvs_classify_identical() {
        let index = crate::index::Index::new(0x01);
        let vv = index.state_vv();
        assert_eq!(
            classify_structural(&vv, &vv.encode()),
            StructuralComparison::Identical,
        );
    }

    /// An undecodable peer Index VV must NOT panic — it classifies `Concurrent`, the
    /// safe over-approximation that forces a converging structural re-exchange.
    #[test]
    fn structural_undecodable_index_vv_classifies_concurrent() {
        let index = crate::index::Index::new(0x01);
        assert_eq!(
            classify_structural(&index.state_vv(), b"not a valid version vector"),
            StructuralComparison::Concurrent,
        );
    }
}
