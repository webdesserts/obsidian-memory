//! The catalog digest: a compact whole-vault fingerprint two replicas compare in
//! one round-trip to decide whether they are fully in sync (the P3 compare
//! protocol's Prune-0).
//!
//! A no-op sync — two replicas already holding identical state — must not cost
//! O(document-count) wire payload. The digest is the cheap discriminator: if two
//! replicas' catalog digests are byte-equal, their merged state is identical and
//! the sync ends with zero further transfer; if they differ, the handshake falls
//! through to the per-document version compare.
//!
//! It is computed from the Index ALONE — no content `.loro` is opened. The
//! per-document input is the denormalized `content_version` fingerprint already
//! maintained on each file node (set at registration, bumped on edit/apply, and
//! boot-repaired against the authoritative `state_vv()`), so the digest is a single
//! cheap Index scan over the alive file nodes.

use super::Index;
use loro::TreeID;
use uuid::Uuid;

impl Index {
    /// A compact whole-vault digest over the catalog's structural version and every
    /// alive document's content fingerprint — the P3 compare protocol's fast-path
    /// discriminator. Two replicas with identical merged state produce byte-equal
    /// digests; a single comparison ends a no-op exchange.
    ///
    /// The formula (spec §12.3):
    ///
    /// ```text
    /// blake3( index_vv.encode()  ++  sorted_by_uuid[ uuid_bytes ++ content_version ] )
    /// ```
    ///
    /// Why each term:
    ///
    /// - **`index_vv` (the Index's `state_vv`)** folds ALL structural state —
    ///   creates, renames, moves, deletes — into the digest. A pure move leaves every
    ///   document's `content_version` unchanged, so without this term a move would be
    ///   invisible to the digest and a peer would never learn it via the fast path.
    ///   Including the Index VV makes any structural change shift the digest.
    /// - **Per-alive-file-node `(uuid, content_version)`** captures content
    ///   divergence: an edit bumps the changed document's `content_version`, which
    ///   shifts the digest. Folders contribute no per-node entry (they carry no
    ///   content); their structural state is already covered by the Index VV.
    ///   Tombstoned nodes contribute nothing — their documents are deleted in the
    ///   merged state.
    /// - **Sorted by UUID** so the digest is independent of `tree.nodes()` iteration
    ///   order. Determinism is the whole point: two replicas at the same converged
    ///   state must agree byte-for-byte, or the fast path would perpetually miss and
    ///   every sync would fall through to a full-state exchange.
    ///
    /// O(alive-documents) local compute, no per-content-doc opens. The digest reads
    /// the cheap denormalized cache; its correctness as a fast-path hint rests on
    /// that cache being maintained + boot-repaired (a P1 guarantee), with the
    /// per-document VV compare as the ground truth on a miss.
    pub fn catalog_digest(&self) -> [u8; 32] {
        let mut hasher = blake3::Hasher::new();
        hasher.update(&self.state_vv().encode());

        // Collect `(uuid, content_version)` for every alive file node, then sort by
        // UUID so the hash is independent of tree iteration order.
        let mut entries: Vec<(Uuid, [u8; 32])> = self
            .live_file_nodes()
            .into_iter()
            .filter_map(|(uuid, node_id)| {
                self.node_content_version(&node_id)
                    .map(|version| (uuid, version))
            })
            .collect();
        entries.sort_unstable_by_key(|(uuid, _)| *uuid);

        for (uuid, version) in entries {
            hasher.update(uuid.as_bytes());
            hasher.update(&version);
        }

        *hasher.finalize().as_bytes()
    }

    /// Every alive FILE node's `(uuid, TreeID)` — the shared catalog-scan primitive
    /// for the compare protocol.
    ///
    /// Walks the tree once, keeping only alive nodes typed `"file"` with a parseable
    /// `uuid`. Folders and tombstoned nodes are excluded (the same live/file
    /// discrimination [`Index::scan_structural_nodes`] uses), so the result mirrors
    /// exactly the set of documents that syncs. The digest (Chunk A) reads each
    /// node's `content_version` through this; the manifest's local-side scan (Chunk B)
    /// reads each node's authoritative `version()`.
    pub(crate) fn live_file_nodes(&self) -> Vec<(Uuid, TreeID)> {
        let tree = self.index_tree();
        let mut nodes = Vec::new();

        for node_id in tree.nodes() {
            if tree.is_node_deleted(&node_id).unwrap_or(true) {
                continue;
            }
            let Ok(meta) = tree.get_meta(node_id) else {
                continue;
            };
            if Self::tree_meta_string(&meta, super::TREE_META_TYPE).as_deref() != Some("file") {
                continue;
            }
            if let Some(uuid) = Self::tree_meta_string(&meta, super::TREE_META_UUID)
                .and_then(|s| Uuid::parse_str(&s).ok())
            {
                nodes.push((uuid, node_id));
            }
        }

        nodes
    }
}

#[cfg(test)]
mod tests {
    use crate::Index;
    use uuid::Uuid;

    /// The sorted-by-uuid catalog hash is order-independent: hashing the same set of
    /// `(uuid, content_version)` entries in two different input orders yields the same
    /// digest. This is the determinism property `catalog_digest` relies on — without
    /// the sort, two replicas that built their tree via different op orders would
    /// produce different digests and perpetually miss the fast path. A fast white-box
    /// regression catch independent of the async vault harness.
    #[test]
    fn sorted_catalog_hash_is_order_independent() {
        let index_vv = Index::new(0x01).state_vv().encode();

        let entries: Vec<(Uuid, [u8; 32])> = vec![
            (Uuid::from_u128(3), [0xAA; 32]),
            (Uuid::from_u128(1), [0xBB; 32]),
            (Uuid::from_u128(2), [0xCC; 32]),
        ];
        let mut shuffled = entries.clone();
        shuffled.reverse();

        assert_eq!(
            hash_catalog(&index_vv, entries),
            hash_catalog(&index_vv, shuffled),
            "the digest must be independent of the order the catalog entries are gathered in"
        );
    }

    /// Mirror of `Index::catalog_digest`'s hashing step over an explicit entry set, so
    /// the determinism property can be exercised without standing up vaults.
    fn hash_catalog(index_vv: &[u8], mut entries: Vec<(Uuid, [u8; 32])>) -> [u8; 32] {
        entries.sort_unstable_by_key(|(uuid, _)| *uuid);
        let mut hasher = blake3::Hasher::new();
        hasher.update(index_vv);
        for (uuid, version) in entries {
            hasher.update(uuid.as_bytes());
            hasher.update(&version);
        }
        *hasher.finalize().as_bytes()
    }
}
