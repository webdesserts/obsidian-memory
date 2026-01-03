//! Shared utilities for note tools.

use obsidian_fs::normalize_note_reference;

use crate::graph::GraphIndex;
use crate::storage::{Storage, StorageError};

/// Resolve a note reference to a memory URI.
///
/// Handles wiki-links (`[[Note]]`), memory URIs (`memory:path/Note`), and plain names.
/// Uses the graph index to find notes in subdirectories when given just a name.
///
/// Returns `(memory_uri, exists)` where:
/// - `memory_uri` is the resolved path (without `.md` extension)
/// - `exists` indicates whether the note was found
///
/// # Resolution Order
/// 1. If the reference includes a path (contains `/`), try that exact path first
/// 2. Look up the note name in the graph index to find its actual location
/// 3. Fall back to the normalized path (for new notes that don't exist yet)
pub async fn resolve_note_uri<S: Storage>(
    storage: &S,
    graph: &GraphIndex,
    note_ref: &str,
) -> Result<(String, bool), StorageError> {
    let normalized = normalize_note_reference(note_ref);

    // First check if the reference includes a path
    if normalized.path.contains('/') {
        // Try the exact path
        if storage.exists(&normalized.path).await? {
            return Ok((normalized.path, true));
        }
    }

    // Try to find in graph index by name
    if let Some(graph_path) = graph.get_path(&normalized.name) {
        let uri = graph_path
            .to_string_lossy()
            .strip_suffix(".md")
            .unwrap_or(&graph_path.to_string_lossy())
            .to_string();
        return Ok((uri, true));
    }

    // Not found - return the normalized path
    Ok((normalized.path, false))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::FileStorage;
    use std::collections::HashSet;
    use std::path::PathBuf;
    use tempfile::TempDir;
    use tokio::fs;

    async fn create_test_env() -> (TempDir, FileStorage, GraphIndex) {
        let temp_dir = TempDir::new().unwrap();
        let storage = FileStorage::new(temp_dir.path().to_path_buf());
        let graph = GraphIndex::new();
        (temp_dir, storage, graph)
    }

    #[tokio::test]
    async fn test_resolve_plain_name_at_root() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        fs::write(temp_dir.path().join("test.md"), "content")
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        let (uri, exists) = resolve_note_uri(&storage, &graph, "test")
            .await
            .unwrap();

        assert_eq!(uri, "test");
        assert!(exists);
    }

    #[tokio::test]
    async fn test_resolve_plain_name_in_subdirectory() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        fs::create_dir(temp_dir.path().join("knowledge")).await.unwrap();
        fs::write(temp_dir.path().join("knowledge/My Note.md"), "content")
            .await
            .unwrap();
        graph.update_note(
            "My Note",
            PathBuf::from("knowledge/My Note.md"),
            HashSet::new(),
        );

        // Just the name should resolve to the full path
        let (uri, exists) = resolve_note_uri(&storage, &graph, "My Note")
            .await
            .unwrap();

        assert_eq!(uri, "knowledge/My Note");
        assert!(exists);
    }

    #[tokio::test]
    async fn test_resolve_wiki_link() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        fs::write(temp_dir.path().join("test.md"), "content")
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        let (uri, exists) = resolve_note_uri(&storage, &graph, "[[test]]")
            .await
            .unwrap();

        assert_eq!(uri, "test");
        assert!(exists);
    }

    #[tokio::test]
    async fn test_resolve_memory_uri_with_path() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        fs::create_dir(temp_dir.path().join("projects")).await.unwrap();
        fs::write(temp_dir.path().join("projects/foo.md"), "content")
            .await
            .unwrap();
        graph.update_note("foo", PathBuf::from("projects/foo.md"), HashSet::new());

        let (uri, exists) = resolve_note_uri(&storage, &graph, "memory:projects/foo")
            .await
            .unwrap();

        assert_eq!(uri, "projects/foo");
        assert!(exists);
    }

    #[tokio::test]
    async fn test_resolve_nonexistent_returns_normalized_path() {
        let (_temp_dir, storage, graph) = create_test_env().await;

        let (uri, exists) = resolve_note_uri(&storage, &graph, "New Note")
            .await
            .unwrap();

        assert_eq!(uri, "New Note");
        assert!(!exists);
    }

    #[tokio::test]
    async fn test_resolve_exact_path_takes_precedence() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        // Create two notes with same name in different directories
        fs::create_dir(temp_dir.path().join("a")).await.unwrap();
        fs::create_dir(temp_dir.path().join("b")).await.unwrap();
        fs::write(temp_dir.path().join("a/note.md"), "content a")
            .await
            .unwrap();
        fs::write(temp_dir.path().join("b/note.md"), "content b")
            .await
            .unwrap();

        // Graph has one path
        graph.update_note("note", PathBuf::from("a/note.md"), HashSet::new());

        // Explicit path should take precedence over graph lookup
        let (uri, exists) = resolve_note_uri(&storage, &graph, "b/note")
            .await
            .unwrap();

        assert_eq!(uri, "b/note");
        assert!(exists);
    }
}
