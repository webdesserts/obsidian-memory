//! NoteDocument: Loro document wrapper for a single markdown note.
//!
//! Each note is represented as a Loro document with:
//! - `_meta`: LoroMap for internal sync metadata (doc_id, path)
//! - `frontmatter`: LoroMap for user's YAML frontmatter
//! - `body`: LoroText for markdown content
//!
//! The `_meta.doc_id` field tracks document lineage for divergent history detection.
//! The `_meta.path` field allows detecting file moves/renames during reconciliation.

use crate::markdown;
use crate::PeerId;
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
    /// The peer_id must be set before any operations to ensure consistent version vectors.
    pub fn new(path: &str, peer_id: PeerId) -> Self {
        let doc = LoroDoc::new();
        doc.set_peer_id(peer_id.as_u64()).ok();

        // Set path metadata only - doc_id comes from imported content or from_markdown()
        let meta = doc.get_map("_meta");
        meta.insert("path", path).ok();
        doc.commit();

        Self {
            doc,
            path: path.to_string(),
        }
    }

    /// Create a NoteDocument by importing from existing Loro bytes.
    ///
    /// The peer_id is set before import so any new operations (like path metadata updates)
    /// are attributed to this peer. Imported operations preserve their original peer IDs.
    pub fn from_bytes(path: &str, bytes: &[u8], peer_id: PeerId) -> Result<Self> {
        debug!(
            path = %path,
            bytes_len = bytes.len(),
            "loro_from_bytes: starting import"
        );

        let doc = LoroDoc::new();
        doc.set_peer_id(peer_id.as_u64()).ok();
        doc.import(bytes).map_err(|e| {
            error!(
                path = %path,
                bytes_len = bytes.len(),
                error = %e,
                "loro_from_bytes FAILED"
            );
            DocumentError::Loro(e.to_string())
        })?;

        let body_len = doc.get_text("body").len_unicode();
        debug!(
            path = %path,
            body_len = body_len,
            "loro_from_bytes: import complete"
        );

        // Update path metadata (this is intentional - records the current path)
        let meta = doc.get_map("_meta");
        meta.insert("path", path)
            .map_err(|e| DocumentError::Loro(e.to_string()))?;
        doc.commit();

        Ok(Self {
            doc,
            path: path.to_string(),
        })
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
        let meta = self.doc.get_map("_meta");
        let value = meta.get_deep_value();
        if let loro::LoroValue::Map(map) = value {
            if let Some(loro::LoroValue::String(s)) = map.get("path") {
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
        let meta = self.doc.get_map("_meta");
        let value = meta.get_deep_value();
        if let loro::LoroValue::Map(map) = value {
            if let Some(loro::LoroValue::String(s)) = map.get("doc_id") {
                return Some(s.to_string());
            }
        }
        None
    }

    /// Update the path stored in metadata.
    ///
    /// Called when a file move is detected during reconciliation.
    pub fn update_path(&mut self, new_path: &str) -> Result<()> {
        let meta = self.doc.get_map("_meta");
        meta.insert("path", new_path)
            .map_err(|e| DocumentError::Loro(e.to_string()))?;
        self.path = new_path.to_string();
        self.doc.commit();
        Ok(())
    }

    /// Get the frontmatter container
    pub fn frontmatter(&self) -> LoroMap {
        self.doc.get_map("frontmatter")
    }

    /// Get the body container
    pub fn body(&self) -> LoroText {
        self.doc.get_text("body")
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
    /// The peer_id must be set before any operations to ensure consistent version vectors.
    pub fn from_markdown(path: &str, content: &str, peer_id: PeerId) -> Result<Self> {
        let doc = LoroDoc::new();
        doc.set_peer_id(peer_id.as_u64()).ok();
        let parsed = markdown::parse(content);

        // Set internal metadata with unique doc_id
        let meta = doc.get_map("_meta");
        meta.insert("doc_id", Uuid::new_v4().to_string())
            .map_err(|e| DocumentError::Loro(e.to_string()))?;
        meta.insert("path", path)
            .map_err(|e| DocumentError::Loro(e.to_string()))?;

        // Set frontmatter
        if let Some(fm) = parsed.frontmatter {
            let frontmatter = doc.get_map("frontmatter");
            for (key, value) in fm {
                let json_value = serde_json::to_value(&value)
                    .map_err(|e| DocumentError::Serialization(e.to_string()))?;
                frontmatter
                    .insert(&key, json_value)
                    .map_err(|e| DocumentError::Loro(e.to_string()))?;
            }
        }

        // Set body
        let body = doc.get_text("body");
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
            let items: std::result::Result<Vec<_>, _> = arr.iter().map(loro_value_to_json).collect();
            Ok(serde_json::Value::Array(items?))
        }
        loro::LoroValue::Map(map) => {
            let obj: std::result::Result<serde_json::Map<String, serde_json::Value>, _> = map
                .iter()
                .map(|(k, v)| Ok((k.clone(), loro_value_to_json(v)?)))
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

    fn test_peer_id() -> PeerId {
        PeerId::from(12345u64)
    }

    #[test]
    fn test_new_document() {
        let doc = NoteDocument::new("test.md", test_peer_id());
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

        let doc = NoteDocument::from_markdown("test.md", content, test_peer_id()).unwrap();
        assert!(doc.to_markdown().contains("title:"));
        assert!(doc.to_markdown().contains("# Hello"));
    }

    #[test]
    fn test_sync_between_documents() {
        // Create two documents
        let doc1 = NoteDocument::from_markdown("test.md", "Hello", test_peer_id()).unwrap();
        let mut doc2 = NoteDocument::new("test.md", test_peer_id());

        // Sync from doc1 to doc2
        let snapshot = doc1.export_snapshot();
        doc2.import(&snapshot).unwrap();

        assert_eq!(doc2.body().to_string(), "Hello");
    }

    #[test]
    fn test_update_body_with_update_by_line() {
        // Test that update_body (using update_by_line) works correctly
        let doc = NoteDocument::from_markdown("test.md", "Hello World", test_peer_id()).unwrap();
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
        let doc = NoteDocument::from_markdown("test.md", "Hello", test_peer_id()).unwrap();

        let changed = doc.update_body("Hello").unwrap();

        assert!(!changed, "Should not detect change for same content");
        assert_eq!(doc.body().to_string(), "Hello");
    }
}
