//! ContentDoc: Loro document wrapper for a single markdown note's content.
//!
//! Each note's content is represented as a Loro document with:
//! - `_meta`: LoroMap for internal sync metadata (just `doc_id` — the UUID)
//! - `frontmatter`: LoroMap for the user's YAML frontmatter
//! - `body`: LoroText for markdown content
//!
//! A `ContentDoc` is **location-agnostic**: it carries the document's UUID identity
//! but NOT its path. A document's location lives in the Index (the registry tree),
//! not in its content doc. This is what makes a move write zero content operations
//! (INV-1) — moving a file is a pure structural change in the Index; the content
//! doc, addressed on disk by its UUID, is untouched.
//!
//! The `_meta.doc_id` field is the UUID minted at creation. It is the document's
//! sole identity: two replicas converge on the same logical document iff they share
//! a doc_id, and a same-UUID merge is always a normal CRDT merge.

use crate::markdown;
use loro::{
    ExportMode, Frontiers, ImportStatus, LoroDoc, LoroMap, LoroText, UpdateOptions, VersionVector,
};
use std::collections::{HashMap, HashSet};
use thiserror::Error;
use tracing::{debug, error, warn};
use uuid::Uuid;

/// Surface a loro import that landed ops with unsatisfied causal dependencies.
///
/// `LoroDoc::import` parks such ops (it neither applies nor errors on them) and
/// reports them in `ImportStatus.pending`. We previously discarded the status at
/// every callsite, making a "delta arrived but its ancestor op is missing"
/// condition invisible. This is the shared sink for that signal — logging only,
/// no control-flow change: the parked ops apply once their deps land via a later
/// exchange, and boot reconciliation backstops anything still stranded. `ctx` names
/// the import site (e.g. a document UUID) for the log.
pub(crate) fn warn_if_pending(status: &ImportStatus, ctx: &str) {
    if let Some(pending) = &status.pending {
        warn!(
            "loro import for {} has pending ops with unsatisfied dependencies: {:?}",
            ctx, pending
        );
    }
}

#[derive(Debug, Error)]
pub enum DocumentError {
    #[error("Loro error: {0}")]
    Loro(String),

    #[error("Serialization error: {0}")]
    Serialization(String),
}

pub type Result<T> = std::result::Result<T, DocumentError>;

// Loro container and field names — changing these breaks existing .loro files.
// The container-names format test below trips if a VALUE (not just the const name)
// changes.

/// Loro map container for internal sync metadata (the `doc_id` UUID).
pub const META_CONTAINER: &str = "_meta";
/// Field within `_meta` that holds the document's UUID — its sole identity.
pub const META_DOC_ID: &str = "doc_id";
/// Loro map container for the user's YAML frontmatter.
pub const FRONTMATTER_CONTAINER: &str = "frontmatter";
/// Loro text container for markdown body content.
pub const BODY_CONTAINER: &str = "body";

/// A single note's content as a Loro document.
///
/// Identity is the UUID stored in `_meta.doc_id`; the doc carries no path.
#[derive(Clone)]
pub struct ContentDoc {
    doc: LoroDoc,
}

impl ContentDoc {
    /// Create a ContentDoc by importing existing Loro bytes.
    ///
    /// The author (a Loro peer id) is set before import so any subsequent local ops
    /// attribute to this independent replica; imported ops keep their original
    /// authors. The UUID is read from the imported `_meta.doc_id` — this method
    /// neither takes nor records a path (the doc is location-agnostic, OQ-3).
    pub fn from_bytes(bytes: &[u8], author: u64) -> Result<Self> {
        debug!(
            bytes_len = bytes.len(),
            "content_doc_from_bytes: starting import"
        );

        let doc = LoroDoc::new();
        doc.set_peer_id(author).ok();
        doc.import(bytes).map_err(|e| {
            error!(bytes_len = bytes.len(), error = %e, "content_doc_from_bytes FAILED");
            DocumentError::Loro(e.to_string())
        })?;

        let body_len = doc.get_text(BODY_CONTAINER).len_unicode();
        debug!(
            body_len = body_len,
            "content_doc_from_bytes: import complete"
        );

        Ok(Self { doc })
    }

    /// Get the document's UUID (its lineage identity).
    ///
    /// Returns `None` only for a malformed doc missing the `_meta.doc_id` field.
    pub fn doc_id(&self) -> Option<String> {
        let meta = self.doc.get_map(META_CONTAINER);
        let value = meta.get_deep_value();
        if let loro::LoroValue::Map(map) = value
            && let Some(loro::LoroValue::String(s)) = map.get(META_DOC_ID)
        {
            return Some(s.to_string());
        }
        None
    }

    /// Get the frontmatter container
    pub fn frontmatter(&self) -> LoroMap {
        self.doc.get_map(FRONTMATTER_CONTAINER)
    }

    /// Get the body container
    pub fn body(&self) -> LoroText {
        self.doc.get_text(BODY_CONTAINER)
    }

    /// Build a content doc from markdown.
    ///
    /// Mints a fresh UUID into `_meta.doc_id` to track document lineage across syncs.
    /// The author (a Loro peer id) is set so operations are attributed to this
    /// independent replica (required for correct CRDT merge — see
    /// [[Loro Peer ID Semantics]]).
    pub fn from_markdown(content: &str, author: u64) -> Result<Self> {
        let doc = LoroDoc::new();
        doc.set_peer_id(author).ok();
        let parsed = markdown::parse(content);

        // Set internal metadata with a unique doc_id (the document's only identity).
        let meta = doc.get_map(META_CONTAINER);
        meta.insert(META_DOC_ID, Uuid::new_v4().to_string())
            .map_err(|e| DocumentError::Loro(e.to_string()))?;

        // Set frontmatter
        if let Some(fm) = parsed.frontmatter {
            let frontmatter = doc.get_map(FRONTMATTER_CONTAINER);
            for (key, value) in fm {
                let json_value = serde_json::to_value(&value)
                    .map_err(|e| DocumentError::Serialization(e.to_string()))?;
                frontmatter
                    .insert(&key, json_value)
                    .map_err(|e| DocumentError::Loro(e.to_string()))?;
            }
        }

        // Set body
        let body = doc.get_text(BODY_CONTAINER);
        body.insert(0, &parsed.body)
            .map_err(|e| DocumentError::Loro(e.to_string()))?;

        doc.commit();

        Ok(Self { doc })
    }

    /// Export to markdown string.
    ///
    /// Frontmatter fields are emitted in a deterministic sorted order (see
    /// `markdown::serialize`), so this is a pure function of the document's logical
    /// state — the foundation of byte-identical convergence (INV-4).
    pub fn to_markdown(&self) -> String {
        let frontmatter = self.get_frontmatter_map();
        let body = self.body().to_string();

        markdown::serialize(frontmatter.as_ref(), &body)
    }

    /// Get frontmatter as a map.
    ///
    /// Key ordering here is irrelevant to determinism — `markdown::serialize` sorts
    /// the keys before emitting YAML, so the materialized markdown is independent of
    /// this map's iteration order (INV-4). Returns `None` for empty frontmatter.
    fn get_frontmatter_map(&self) -> Option<HashMap<String, serde_yaml::Value>> {
        let fm = self.frontmatter();
        let value = fm.get_deep_value();

        if let loro::LoroValue::Map(map) = value {
            if map.is_empty() {
                return None;
            }
            let mut result = HashMap::new();
            for (key, value) in map.iter() {
                if let Ok(yaml_value) = loro_value_to_yaml(value) {
                    result.insert(key.clone(), yaml_value);
                }
            }
            Some(result)
        } else {
            None
        }
    }

    /// Get current version vector.
    ///
    /// This `state_vv()` is the sole authority for the document's content state —
    /// the basis for all version comparison in the sync protocol.
    pub fn version(&self) -> VersionVector {
        self.doc.state_vv()
    }

    /// Get current frontiers (tips of the DAG)
    pub fn frontiers(&self) -> Frontiers {
        self.doc.state_frontiers()
    }

    /// Export full snapshot.
    ///
    /// Returns `Result` rather than panicking: a loro export failure (e.g. an
    /// internal encoding error) is surfaced to the caller so a single bad
    /// document can't take down the daemon.
    pub fn export_snapshot(&self) -> Result<Vec<u8>> {
        self.doc
            .export(ExportMode::Snapshot)
            .map_err(|e| DocumentError::Loro(e.to_string()))
    }

    /// Export updates since a version.
    ///
    /// Returns `Result` rather than panicking: a loro export failure is
    /// surfaced to the caller so a single bad document can't take down the
    /// daemon.
    pub fn export_updates(&self, from: &VersionVector) -> Result<Vec<u8>> {
        self.doc
            .export(ExportMode::updates(from))
            .map_err(|e| DocumentError::Loro(e.to_string()))
    }

    /// Import data from bytes (a CRDT merge into this doc).
    pub fn import(&mut self, data: &[u8]) -> Result<()> {
        let body_len_before = self.body().len_unicode();
        let vv_before = self.version();
        let ctx = self.doc_id().unwrap_or_default();

        debug!(
            doc_id = %ctx,
            body_len = body_len_before,
            data_len = data.len(),
            vv = ?vv_before,
            "content_doc_import: starting"
        );

        let status = self.doc.import(data).map_err(|e| {
            error!(
                doc_id = %ctx,
                body_len = body_len_before,
                data_len = data.len(),
                vv = ?vv_before,
                error = %e,
                "content_doc_import FAILED"
            );
            DocumentError::Loro(e.to_string())
        })?;

        // Surface (but don't act on) ops parked with unsatisfied dependencies.
        warn_if_pending(&status, &ctx);

        let body_len_after = self.body().len_unicode();
        let vv_after = self.version();
        debug!(
            doc_id = %ctx,
            body_len_before = body_len_before,
            body_len_after = body_len_after,
            vv = ?vv_after,
            "content_doc_import: complete"
        );

        Ok(())
    }

    /// Checkout to a specific version (for time travel)
    pub fn checkout(&mut self, frontiers: &Frontiers) {
        self.doc.checkout(frontiers).ok();
    }

    /// Return to latest version
    pub fn checkout_to_latest(&mut self) {
        self.doc.checkout_to_latest();
    }

    /// Commit pending changes
    pub fn commit(&self) {
        self.doc.commit();
    }

    // ========== Debug API Methods ==========

    /// Get the number of changes in the document's oplog.
    pub fn len_changes(&self) -> usize {
        self.doc.len_changes()
    }

    /// Get the number of operations in the document's oplog.
    pub fn len_ops(&self) -> usize {
        self.doc.len_ops()
    }

    /// Update the body text by computing and applying a line-based diff.
    ///
    /// Uses Loro's built-in `update_by_line()` which computes line-based diffs
    /// efficiently. Preserves peer ID by operating on the existing LoroText so the
    /// edit is attributed to this replica.
    pub fn update_body(&self, new_body: &str) -> Result<bool> {
        let body = self.body();
        let old_body = body.to_string();
        let old_len = body.len_unicode();

        if old_body == new_body {
            return Ok(false); // No changes
        }

        debug!(
            old_len = old_len,
            new_len = new_body.chars().count(),
            "update_body: starting update_by_line"
        );

        body.update_by_line(new_body, UpdateOptions::default())
            .map_err(|e| {
                error!(
                    old_len = old_len,
                    new_len = new_body.chars().count(),
                    error = ?e,
                    "update_body FAILED"
                );
                DocumentError::Loro(format!("{:?}", e))
            })?;

        debug!(
            old_len = old_len,
            new_len = body.len_unicode(),
            "update_body: complete"
        );

        Ok(true) // Changes applied (commit happens in caller)
    }

    /// Update frontmatter by comparing and applying changes key-by-key.
    ///
    /// Preserves peer ID by operating on the existing LoroMap.
    pub fn update_frontmatter(
        &self,
        new_fm: Option<&HashMap<String, serde_yaml::Value>>,
    ) -> Result<bool> {
        let fm = self.frontmatter();

        // Get existing keys from LoroMap
        let old_map = fm.get_deep_value();
        let old_keys: HashSet<String> = match &old_map {
            loro::LoroValue::Map(m) => m.keys().cloned().collect(),
            _ => HashSet::new(),
        };

        let new_map = new_fm.cloned().unwrap_or_default();
        let new_keys: HashSet<String> = new_map.keys().cloned().collect();

        let mut changed = false;

        // Delete removed keys
        for key in old_keys.difference(&new_keys) {
            fm.delete(key)
                .map_err(|e| DocumentError::Loro(e.to_string()))?;
            changed = true;
        }

        // Insert/update keys
        for (key, value) in &new_map {
            let json_value = serde_json::to_value(value)
                .map_err(|e| DocumentError::Serialization(e.to_string()))?;

            // Get old value and convert to comparable format
            let old_json = match &old_map {
                loro::LoroValue::Map(m) => m.get(key).and_then(|v| loro_value_to_json(v).ok()),
                _ => None,
            };

            // Only update if value changed
            if old_json.as_ref() != Some(&json_value) {
                fm.insert(key, json_value)
                    .map_err(|e| DocumentError::Loro(e.to_string()))?;
                changed = true;
            }
        }

        Ok(changed) // Commit happens in caller
    }
}

/// Convert LoroValue to serde_json::Value for comparison
fn loro_value_to_json(value: &loro::LoroValue) -> std::result::Result<serde_json::Value, ()> {
    match value {
        loro::LoroValue::Null => Ok(serde_json::Value::Null),
        loro::LoroValue::Bool(b) => Ok(serde_json::Value::Bool(*b)),
        loro::LoroValue::I64(n) => Ok(serde_json::json!(*n)),
        loro::LoroValue::Double(n) => Ok(serde_json::json!(*n)),
        loro::LoroValue::String(s) => Ok(serde_json::Value::String(s.to_string())),
        loro::LoroValue::List(arr) => {
            let items: std::result::Result<Vec<_>, ()> =
                arr.iter().map(loro_value_to_json).collect();
            Ok(serde_json::Value::Array(items?))
        }
        loro::LoroValue::Map(map) => {
            let obj: std::result::Result<serde_json::Map<String, serde_json::Value>, ()> = map
                .iter()
                .map(|(k, v)| -> std::result::Result<_, ()> {
                    Ok((k.clone(), loro_value_to_json(v)?))
                })
                .collect();
            Ok(serde_json::Value::Object(obj?))
        }
        _ => Ok(serde_json::Value::Null), // Container types - treat as null
    }
}

/// Convert Loro value to YAML value
fn loro_value_to_yaml(value: &loro::LoroValue) -> std::result::Result<serde_yaml::Value, ()> {
    match value {
        loro::LoroValue::Null => Ok(serde_yaml::Value::Null),
        loro::LoroValue::Bool(b) => Ok(serde_yaml::Value::Bool(*b)),
        loro::LoroValue::I64(n) => Ok(serde_yaml::Value::Number((*n).into())),
        loro::LoroValue::Double(n) => Ok(serde_yaml::Value::Number((*n).into())),
        loro::LoroValue::String(s) => Ok(serde_yaml::Value::String(s.to_string())),
        loro::LoroValue::List(list) => {
            let items: std::result::Result<Vec<_>, _> =
                list.iter().map(loro_value_to_yaml).collect();
            Ok(serde_yaml::Value::Sequence(items?))
        }
        loro::LoroValue::Map(map) => {
            let mut mapping = serde_yaml::Mapping::new();
            for (k, v) in map.iter() {
                mapping.insert(serde_yaml::Value::String(k.clone()), loro_value_to_yaml(v)?);
            }
            Ok(serde_yaml::Value::Mapping(mapping))
        }
        _ => Err(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // Two distinct device author ids (Loro peer ids).
    const AUTHOR_A: u64 = 0x0101_0101_0101_0101;
    const AUTHOR_B: u64 = 0x0202_0202_0202_0202;

    #[test]
    fn test_document_container_names() {
        // These literal strings ARE the on-disk .loro document wire format: the
        // container/field names under which every note's meta, frontmatter, and
        // body are stored. Changing a VALUE (not just the const name) makes this
        // build write/read note documents under different keys than every other
        // build in the fleet, silently breaking cross-version .loro compatibility.
        //
        // A round-trip test can't catch such a rename: it uses the SAME renamed
        // const on both the writer and reader side, so it stays green while the
        // format diverges fleet-wide. Only asserting the literal values catches it.
        //
        // OQ-3: `_meta` carries ONLY `doc_id` (the UUID). There is deliberately no
        // `path` field — the content doc is location-agnostic.
        assert_eq!(META_CONTAINER, "_meta");
        assert_eq!(META_DOC_ID, "doc_id");
        assert_eq!(FRONTMATTER_CONTAINER, "frontmatter");
        assert_eq!(BODY_CONTAINER, "body");
    }

    #[test]
    fn test_from_markdown() {
        let content = r#"---
title: Test
---

# Hello

World"#;

        let doc = ContentDoc::from_markdown(content, AUTHOR_A).unwrap();
        assert!(doc.to_markdown().contains("title:"));
        assert!(doc.to_markdown().contains("# Hello"));
        // from_markdown mints a UUID as the doc's identity.
        assert!(doc.doc_id().is_some());
    }

    #[test]
    fn test_update_body_with_update_by_line() {
        let doc = ContentDoc::from_markdown("Hello World", AUTHOR_A).unwrap();
        assert_eq!(doc.body().to_string(), "Hello World");

        let changed = doc.update_body("Hello Universe").unwrap();
        doc.commit();

        assert!(changed, "Should detect change");
        assert_eq!(doc.body().to_string(), "Hello Universe");
    }

    #[test]
    fn test_update_body_no_change() {
        let doc = ContentDoc::from_markdown("Hello", AUTHOR_A).unwrap();

        let changed = doc.update_body("Hello").unwrap();

        assert!(!changed, "Should not detect change for same content");
        assert_eq!(doc.body().to_string(), "Hello");
    }

    /// **AC-INV-4-DET** — the headline determinism guard for this chunk.
    ///
    /// One logical document state — frontmatter with three fields plus a body — is
    /// reachable two ways: replica A authors the fields in one order, replica B in
    /// the reverse order, then each imports the other's updates so both hold the
    /// identical merged state. `to_markdown()` MUST be byte-equal on both sides.
    ///
    /// This catches the INV-4/B3 frontmatter-ordering bug directly: the frontmatter
    /// surfaces from the `LoroMap` into a `HashMap` with process-randomized
    /// iteration order, and `serde_yaml` does not sort keys. On a naive serialize
    /// the two replicas would emit the three fields in different orders and this
    /// assertion would fail. It passes only because `markdown::serialize` +
    /// `get_frontmatter_map` impose a fixed sorted key order.
    #[test]
    fn test_materialized_markdown_is_byte_equal_under_shuffled_op_order() {
        // Enough frontmatter fields that two independently-built `HashMap`s
        // coincidentally iterating in the same order is vanishingly unlikely
        // (~1/8! ≈ 1/40320). On a naive (non-sorted) serialize the two replicas'
        // field orderings would almost always differ and the byte-equal assertion
        // fails; with the sorted serialize they're always equal. Values are unique
        // per key so per-key LWW converges to one agreed value (the assertion
        // isolates serialize ORDER, not a merge disagreement).
        let fields = [
            ("alpha", "1"),
            ("bravo", "2"),
            ("charlie", "3"),
            ("delta", "4"),
            ("echo", "5"),
            ("foxtrot", "6"),
            ("golf", "7"),
            ("hotel", "8"),
        ];

        // A and B start from a shared base so they share a doc_id and a base VV.
        let base = ContentDoc::from_markdown("# Body\n", AUTHOR_A).unwrap();
        let base_snapshot = base.export_snapshot().unwrap();
        let base_vv = base.version();

        let mut doc_a = ContentDoc::from_bytes(&base_snapshot, AUTHOR_A).unwrap();
        let mut doc_b = ContentDoc::from_bytes(&base_snapshot, AUTHOR_B).unwrap();

        // A authors the fields one at a time in FORWARD order; B authors the SAME
        // fields one at a time in REVERSE order. Authoring each key as its own commit
        // makes the op order on each side genuinely different — "one logical state
        // reached two ways," exactly what the AC requires.
        let mut acc_a: HashMap<String, serde_yaml::Value> = HashMap::new();
        for (k, v) in fields {
            acc_a.insert(k.to_string(), serde_yaml::Value::String(v.to_string()));
            doc_a.update_frontmatter(Some(&acc_a)).unwrap();
            doc_a.commit();
        }

        let mut acc_b: HashMap<String, serde_yaml::Value> = HashMap::new();
        for (k, v) in fields.iter().rev() {
            acc_b.insert(k.to_string(), serde_yaml::Value::String(v.to_string()));
            doc_b.update_frontmatter(Some(&acc_b)).unwrap();
            doc_b.commit();
        }

        // Cross-import so both replicas hold the identical merged state.
        let updates_from_b = doc_b.export_updates(&base_vv).unwrap();
        doc_a.import(&updates_from_b).unwrap();
        let updates_from_a = doc_a.export_updates(&base_vv).unwrap();
        doc_b.import(&updates_from_a).unwrap();

        // The materialized markdown is byte-identical on both replicas — the guard
        // that the HashMap->sorted serialize fix actually landed.
        assert_eq!(
            doc_a.to_markdown(),
            doc_b.to_markdown(),
            "materialized markdown must be byte-equal regardless of frontmatter authoring order"
        );

        // Independently of the cross-replica comparison, the serialized frontmatter
        // keys must be in lexicographic (sorted) order. This is a deterministic
        // catch for a non-sorted serialize: a correct impl ALWAYS sorts, so any
        // out-of-order pair fails here regardless of how the HashMap happened to
        // iterate.
        let md = doc_a.to_markdown();
        let key_positions: Vec<usize> = fields
            .iter()
            .map(|(k, _)| {
                md.find(&format!("{}:", k)).unwrap_or_else(|| {
                    panic!("frontmatter key {k} missing from materialized markdown")
                })
            })
            .collect();
        assert!(
            key_positions.windows(2).all(|w| w[0] < w[1]),
            "frontmatter keys must be emitted in sorted order, got positions {key_positions:?}"
        );
    }

    /// **AC-INV-2** (at the ContentDoc level) — same-doc concurrent merge.
    ///
    /// Two replicas, sharing a vault but authoring under distinct CRDT author ids,
    /// edit the body offline (no syncing between the edits), then cross-import. Both
    /// edits are present, there is no interleaving corruption, and the merged
    /// version vector has exactly one entry per author — proof the replicas were
    /// genuinely independent.
    ///
    /// This is the regression tripwire for the shared-author corruption class (see
    /// [[Loro Peer ID Semantics]]): if a future change reverts to a shared author,
    /// the concurrent edits collide on `(peer, counter)` OpIds, the merged version
    /// vector collapses to a single entry, and `vv.iter().count()` drops to 1.
    #[test]
    fn test_independent_replicas_have_distinct_authors_and_converge() {
        // Device A creates the note; device B starts from A's history but authors
        // new ops under its own author id (two devices sharing a vault).
        let doc_a = ContentDoc::from_markdown("base", AUTHOR_A).unwrap();
        let base_snapshot = doc_a.export_snapshot().unwrap();
        let mut doc_b = ContentDoc::from_bytes(&base_snapshot, AUTHOR_B).unwrap();

        let base_vv = doc_a.version();

        // Both edit independently while offline (no syncing between the edits).
        doc_a.update_body("base edited by A").unwrap();
        doc_a.commit();
        doc_b.update_body("base edited by B").unwrap();
        doc_b.commit();

        // Cross-import each other's updates since the shared base.
        let updates_from_b = doc_b.export_updates(&base_vv).unwrap();
        let mut doc_a = doc_a;
        doc_a.import(&updates_from_b).unwrap();
        let updates_from_a = doc_a.export_updates(&base_vv).unwrap();
        doc_b.import(&updates_from_a).unwrap();

        // Both replicas converge to identical content.
        assert_eq!(doc_a.to_markdown(), doc_b.to_markdown());

        // The merged version vector has one entry per device — proof the two
        // replicas authored under distinct Loro peer ids.
        assert_eq!(
            doc_a.version().iter().count(),
            2,
            "merged version vector should have one entry per device author"
        );
    }
}
