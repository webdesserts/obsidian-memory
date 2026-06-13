//! NoteDocument: Loro document wrapper for a single markdown note.
//!
//! Each note is represented as a Loro document with:
//! - `_meta`: LoroMap for internal sync metadata (doc_id, path)
//! - `frontmatter`: LoroMap for user's YAML frontmatter
//! - `body`: LoroText for markdown content
//!
//! The `_meta.doc_id` field tracks document lineage for divergent history detection.
//! The `_meta.path` field allows detecting file moves/renames during reconciliation.

use crate::PeerId;
use crate::markdown;
use loro::{ExportMode, Frontiers, LoroDoc, LoroMap, LoroText, UpdateOptions, VersionVector};
use std::collections::hash_map::DefaultHasher;
use std::collections::{HashMap, HashSet};
use std::hash::{Hash, Hasher};
use thiserror::Error;
use tracing::{debug, error};
use uuid::Uuid;

#[derive(Debug, Error)]
pub enum DocumentError {
    #[error("Loro error: {0}")]
    Loro(String),

    #[error("Serialization error: {0}")]
    Serialization(String),
}

pub type Result<T> = std::result::Result<T, DocumentError>;

// Loro container and field names — changing these breaks existing .loro files.
// Format stability tests in vault.rs will trip if these change.

/// Loro map container for internal sync metadata (doc_id, path).
pub const META_CONTAINER: &str = "_meta";
/// Field within `_meta` that tracks document lineage for divergent history detection.
pub const META_DOC_ID: &str = "doc_id";
/// Field within `_meta` that stores the file path (for detecting moves/renames).
pub const META_PATH: &str = "path";
/// Loro map container for user's YAML frontmatter.
pub const FRONTMATTER_CONTAINER: &str = "frontmatter";
/// Loro text container for markdown body content.
pub const BODY_CONTAINER: &str = "body";

/// A single note (markdown file) as a Loro document
#[derive(Clone)]
pub struct NoteDocument {
    doc: LoroDoc,
    path: String,
}

impl NoteDocument {
    /// Create a new empty document for a path.
    ///
    /// Does NOT set doc_id - this is intended for receiving imported data
    /// or as a container that will receive content via update methods.
    /// Use `from_markdown()` to create a new document with original content and doc_id.
    /// The device's `PeerId` is set as the Loro peer id so operations are attributed
    /// to this independent replica (required for correct CRDT merge — see
    /// [[Loro Peer ID Semantics]]).
    pub fn new(path: &str, author: PeerId) -> Self {
        let doc = LoroDoc::new();
        doc.set_peer_id(author.as_u64()).ok();

        // Set path metadata only - doc_id comes from imported content or from_markdown()
        let meta = doc.get_map(META_CONTAINER);
        meta.insert(META_PATH, path).ok();
        doc.commit();

        Self {
            doc,
            path: path.to_string(),
        }
    }

    /// Create a NoteDocument by importing from existing Loro bytes.
    ///
    /// The device's `PeerId` is set as the Loro peer id before import so any new
    /// operations (like path metadata updates) are attributed to this independent
    /// replica. Imported operations preserve their original author IDs.
    pub fn from_bytes(path: &str, bytes: &[u8], author: PeerId) -> Result<Self> {
        debug!(
            path = %path,
            bytes_len = bytes.len(),
            "loro_from_bytes: starting import"
        );

        let doc = LoroDoc::new();
        doc.set_peer_id(author.as_u64()).ok();
        doc.import(bytes).map_err(|e| {
            error!(
                path = %path,
                bytes_len = bytes.len(),
                error = %e,
                "loro_from_bytes FAILED"
            );
            DocumentError::Loro(e.to_string())
        })?;

        let body_len = doc.get_text(BODY_CONTAINER).len_unicode();
        debug!(
            path = %path,
            body_len = body_len,
            "loro_from_bytes: import complete"
        );

        // Update path metadata (this is intentional - records the current path)
        let meta = doc.get_map(META_CONTAINER);
        meta.insert(META_PATH, path)
            .map_err(|e| DocumentError::Loro(e.to_string()))?;
        doc.commit();

        Ok(Self {
            doc,
            path: path.to_string(),
        })
    }

    /// Create a NoteDocument by importing existing Loro bytes WITHOUT overwriting
    /// the stored `META_PATH`.
    ///
    /// Unlike `from_bytes`, which records the caller-supplied path into `META_PATH`
    /// (correct for reindex/migration where the current path is authoritative), this
    /// variant leaves the document's own stored path intact so `stored_path()` returns
    /// the path the document was originally written under.
    ///
    /// Intended ONLY for read-without-mutating cases: the reconcile orphan-loader (to
    /// recover a deleted file's real path for cleanup/reporting) and stored-path
    /// introspection. Do NOT substitute this for `from_bytes` at the mutating
    /// call-sites (`migrate_document`, `reindex_file`, `load_document`, `needs_reindex`)
    /// — those rely on the `META_PATH` overwrite to record the file's current path.
    ///
    /// The device's `PeerId` is set as the Loro peer id before import so any
    /// subsequent local ops attribute to this replica; imported ops keep their authors.
    pub fn from_bytes_preserve_path(bytes: &[u8], author: PeerId) -> Result<Self> {
        let doc = LoroDoc::new();
        doc.set_peer_id(author.as_u64()).ok();
        doc.import(bytes).map_err(|e| {
            error!(
                bytes_len = bytes.len(),
                error = %e,
                "loro_from_bytes_preserve_path FAILED"
            );
            DocumentError::Loro(e.to_string())
        })?;

        // Seed the local path cache from the document's own stored META_PATH. The
        // empty fallback only applies to meta-less legacy docs, where stored_path()
        // returns None — callers fall back to the doc's hash in that case.
        let mut note = Self {
            doc,
            path: String::new(),
        };
        if let Some(stored) = note.stored_path() {
            note.path = stored;
        }
        Ok(note)
    }

    /// Get the document path (from local cache)
    pub fn path(&self) -> &str {
        &self.path
    }

    /// Get the path stored in the Loro document metadata.
    ///
    /// This may differ from `path()` if the file was moved.
    /// Returns None if metadata is missing (legacy document).
    pub fn stored_path(&self) -> Option<String> {
        let meta = self.doc.get_map(META_CONTAINER);
        let value = meta.get_deep_value();
        if let loro::LoroValue::Map(map) = value {
            if let Some(loro::LoroValue::String(s)) = map.get(META_PATH) {
                return Some(s.to_string());
            }
        }
        None
    }

    /// Get the document's unique ID for lineage tracking.
    ///
    /// Documents created from the same source (via sync) share the same doc_id.
    /// Documents created independently have different doc_ids, indicating divergent history.
    /// Returns None for legacy documents created before doc_id was added.
    pub fn doc_id(&self) -> Option<String> {
        let meta = self.doc.get_map(META_CONTAINER);
        let value = meta.get_deep_value();
        if let loro::LoroValue::Map(map) = value {
            if let Some(loro::LoroValue::String(s)) = map.get(META_DOC_ID) {
                return Some(s.to_string());
            }
        }
        None
    }

    /// Update the path stored in metadata.
    ///
    /// Called when a file move is detected during reconciliation.
    pub fn update_path(&mut self, new_path: &str) -> Result<()> {
        let meta = self.doc.get_map(META_CONTAINER);
        meta.insert(META_PATH, new_path)
            .map_err(|e| DocumentError::Loro(e.to_string()))?;
        self.path = new_path.to_string();
        self.doc.commit();
        Ok(())
    }

    /// Get the frontmatter container
    pub fn frontmatter(&self) -> LoroMap {
        self.doc.get_map(FRONTMATTER_CONTAINER)
    }

    /// Get the body container
    pub fn body(&self) -> LoroText {
        self.doc.get_text(BODY_CONTAINER)
    }

    /// Compute a hash of the document content (frontmatter + body).
    ///
    /// Used to detect if a file was moved vs. deleted+created.
    pub fn content_hash(&self) -> u64 {
        let mut hasher = DefaultHasher::new();
        self.to_markdown().hash(&mut hasher);
        hasher.finish()
    }

    /// Load from markdown content.
    ///
    /// Generates a unique `doc_id` to track document lineage across syncs.
    /// The device's `PeerId` is set as the Loro peer id so operations are attributed
    /// to this independent replica (required for correct CRDT merge — see
    /// [[Loro Peer ID Semantics]]).
    pub fn from_markdown(path: &str, content: &str, author: PeerId) -> Result<Self> {
        let doc = LoroDoc::new();
        doc.set_peer_id(author.as_u64()).ok();
        let parsed = markdown::parse(content);

        // Set internal metadata with unique doc_id
        let meta = doc.get_map(META_CONTAINER);
        meta.insert(META_DOC_ID, Uuid::new_v4().to_string())
            .map_err(|e| DocumentError::Loro(e.to_string()))?;
        meta.insert(META_PATH, path)
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

        Ok(Self {
            doc,
            path: path.to_string(),
        })
    }

    /// Export to markdown string
    pub fn to_markdown(&self) -> String {
        let frontmatter = self.get_frontmatter_map();
        let body = self.body().to_string();

        markdown::serialize(frontmatter.as_ref(), &body)
    }

    /// Get frontmatter as a HashMap
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

    /// Get current version vector
    pub fn version(&self) -> VersionVector {
        self.doc.state_vv()
    }

    /// Get current frontiers (tips of the DAG)
    pub fn frontiers(&self) -> Frontiers {
        self.doc.state_frontiers()
    }

    /// Export full snapshot
    pub fn export_snapshot(&self) -> Vec<u8> {
        self.doc.export(ExportMode::Snapshot).unwrap()
    }

    /// Export updates since a version
    pub fn export_updates(&self, from: &VersionVector) -> Vec<u8> {
        self.doc.export(ExportMode::updates(from)).unwrap()
    }

    /// Import data from bytes
    pub fn import(&mut self, data: &[u8]) -> Result<()> {
        let body_len_before = self.body().len_unicode();
        let vv_before = self.version();

        debug!(
            path = %self.path,
            body_len = body_len_before,
            data_len = data.len(),
            vv = ?vv_before,
            "loro_import: starting"
        );

        self.doc.import(data).map_err(|e| {
            error!(
                path = %self.path,
                body_len = body_len_before,
                data_len = data.len(),
                vv = ?vv_before,
                error = %e,
                "loro_import FAILED"
            );
            DocumentError::Loro(e.to_string())
        })?;

        let body_len_after = self.body().len_unicode();
        let vv_after = self.version();
        debug!(
            path = %self.path,
            body_len_before = body_len_before,
            body_len_after = body_len_after,
            vv = ?vv_after,
            "loro_import: complete"
        );

        // Update local path cache from imported metadata if present
        if let Some(stored) = self.stored_path() {
            self.path = stored;
        }

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
    /// efficiently. Preserves peer ID by operating on existing LoroText.
    pub fn update_body(&self, new_body: &str) -> Result<bool> {
        let body = self.body();
        let old_body = body.to_string();
        let old_len = body.len_unicode();

        if old_body == new_body {
            return Ok(false); // No changes
        }

        debug!(
            path = %self.path,
            old_len = old_len,
            new_len = new_body.chars().count(),
            "update_body: starting update_by_line"
        );

        body.update_by_line(new_body, UpdateOptions::default())
            .map_err(|e| {
                error!(
                    path = %self.path,
                    old_len = old_len,
                    new_len = new_body.chars().count(),
                    error = ?e,
                    "update_body FAILED"
                );
                DocumentError::Loro(format!("{:?}", e))
            })?;

        debug!(
            path = %self.path,
            old_len = old_len,
            new_len = body.len_unicode(),
            "update_body: complete"
        );

        Ok(true) // Changes applied (commit happens in caller)
    }

    /// Update frontmatter by comparing and applying changes key-by-key.
    ///
    /// Preserves peer ID by operating on existing LoroMap.
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

    fn test_author() -> PeerId {
        PeerId::from_bytes([1u8; 32])
    }

    fn test_author_2() -> PeerId {
        PeerId::from_bytes([2u8; 32])
    }

    #[test]
    fn test_new_document() {
        let doc = NoteDocument::new("test.md", test_author());
        assert_eq!(doc.path(), "test.md");
        assert!(doc.body().to_string().is_empty());
    }

    #[test]
    fn test_from_markdown() {
        let content = r#"---
title: Test
---

# Hello

World"#;

        let doc = NoteDocument::from_markdown("test.md", content, test_author()).unwrap();
        assert!(doc.to_markdown().contains("title:"));
        assert!(doc.to_markdown().contains("# Hello"));
    }

    #[test]
    fn test_sync_between_documents() {
        // Create two documents
        let doc1 = NoteDocument::from_markdown("test.md", "Hello", test_author()).unwrap();
        let mut doc2 = NoteDocument::new("test.md", test_author());

        // Sync from doc1 to doc2
        let snapshot = doc1.export_snapshot();
        doc2.import(&snapshot).unwrap();

        assert_eq!(doc2.body().to_string(), "Hello");
    }

    #[test]
    fn test_update_body_with_update_by_line() {
        // Test that update_body (using update_by_line) works correctly
        let doc = NoteDocument::from_markdown("test.md", "Hello World", test_author()).unwrap();
        assert_eq!(doc.body().to_string(), "Hello World");

        // Update the body
        let changed = doc.update_body("Hello Universe").unwrap();
        doc.commit();

        assert!(changed, "Should detect change");
        assert_eq!(doc.body().to_string(), "Hello Universe");
    }

    #[test]
    fn test_update_body_no_change() {
        // Test that update_body returns false when content is the same
        let doc = NoteDocument::from_markdown("test.md", "Hello", test_author()).unwrap();

        let changed = doc.update_body("Hello").unwrap();

        assert!(!changed, "Should not detect change for same content");
        assert_eq!(doc.body().to_string(), "Hello");
    }

    /// Two devices that share a vault but author under distinct `PeerId`s must
    /// edit the same note offline and still converge without OpId collisions.
    ///
    /// This is the regression tripwire for the shared-peer-id corruption bug
    /// (see [[Loro Peer ID Semantics]]): under the old behavior both replicas
    /// authored under the same VaultId, so concurrent offline edits produced
    /// colliding `(peer, counter)` OpIds and the merged version vector collapsed
    /// to a single entry. With per-device authoring the merged version vector
    /// has exactly two entries — one per device — proving the replicas were
    /// genuinely independent. Anchored to the distinct-author invariant: if a
    /// future change reverts to a shared author, `vv.iter().count()` drops to 1
    /// and this fails.
    #[test]
    fn test_independent_replicas_have_distinct_authors_and_converge() {
        let author_a = test_author();
        let author_b = test_author_2();

        // Device A creates the note, then device B starts from A's history but
        // authors new ops under its own PeerId (two devices sharing a vault).
        let doc_a = NoteDocument::from_markdown("note.md", "base", author_a).unwrap();
        let base_snapshot = doc_a.export_snapshot();
        let mut doc_b = NoteDocument::from_bytes("note.md", &base_snapshot, author_b).unwrap();

        let base_vv = doc_a.version();

        // Both edit independently while offline (no syncing between the edits).
        doc_a.update_body("base edited by A").unwrap();
        doc_a.commit();
        doc_b.update_body("base edited by B").unwrap();
        doc_b.commit();

        // Cross-import each other's updates since the shared base.
        let updates_from_b = doc_b.export_updates(&base_vv);
        let mut doc_a = doc_a;
        doc_a.import(&updates_from_b).unwrap();
        let updates_from_a = doc_a.export_updates(&base_vv);
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

    /// Loading an orphaned `.loro` for read-only introspection must surface the
    /// document's own stored path, not clobber it. `from_bytes` overwrites
    /// `META_PATH` with its `path` arg (intentional for reindex/migration), so
    /// the reconcile orphan-loader needs the preserve variant to recover the
    /// real deleted path instead of an empty string.
    #[test]
    fn test_from_bytes_preserve_path_keeps_original_meta_path() {
        // Build a doc at a known path, export it, then reload via the preserve
        // loader and confirm the stored path round-trips.
        let doc = NoteDocument::from_markdown("a/b.md", "# Content", test_author()).unwrap();
        let snapshot = doc.export_snapshot();

        let reloaded = NoteDocument::from_bytes_preserve_path(&snapshot, test_author()).unwrap();
        assert_eq!(
            reloaded.stored_path(),
            Some("a/b.md".to_string()),
            "preserve loader must not overwrite the document's stored META_PATH"
        );
    }
}
