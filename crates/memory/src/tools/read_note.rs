//! ReadNote tool - read note content with content hash for optimistic locking.

use rmcp::model::{CallToolResult, Content, ErrorData};
use serde::Serialize;

use super::common::resolve_note_uri;
use crate::graph::GraphIndex;
use crate::storage::{ContentHash, Storage, StorageError};

/// Response from ReadNote tool.
#[derive(Serialize)]
pub struct ReadNoteResponse {
    /// The content of the note
    pub content: String,
    /// Content hash for optimistic locking - pass this to write_note or edit_note
    pub content_hash: String,
}

/// Execute the ReadNote tool.
///
/// Returns note content and content hash for subsequent writes.
pub async fn execute<S: Storage>(
    storage: &S,
    graph: &GraphIndex,
    note: &str,
) -> Result<CallToolResult, ErrorData> {
    // Resolve the note reference
    let (uri, exists) = resolve_note_uri(storage, graph, note).await.map_err(|e| {
        ErrorData::internal_error(format!("Failed to resolve note: {}", e), None)
    })?;

    if !exists {
        return Err(ErrorData::invalid_params(
            format!("Note not found: {}", note),
            None,
        ));
    }

    // Read the note
    let (content, _metadata) = storage.read(&uri).await.map_err(|e| match e {
        StorageError::NotFound { .. } => {
            // Race condition - file was deleted between resolve and read
            ErrorData::internal_error("Note was deleted during read", None)
        }
        _ => ErrorData::internal_error(format!("Failed to read note: {}", e), None),
    })?;

    // Compute content hash for client to use in subsequent writes
    let content_hash = ContentHash::from_content(&content);

    // Return JSON with content and hash
    let response = ReadNoteResponse {
        content,
        content_hash: content_hash.as_str().to_string(),
    };
    let json = serde_json::to_string(&response)
        .map_err(|e| ErrorData::internal_error(format!("Failed to serialize response: {}", e), None))?;

    Ok(CallToolResult::success(vec![Content::text(json)]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::FileStorage;
    use serde::Deserialize;
    use std::collections::HashSet;
    use std::path::PathBuf;
    use tempfile::TempDir;
    use tokio::fs;

    #[derive(Deserialize)]
    struct TestResponse {
        content: String,
        content_hash: String,
    }

    async fn create_test_env() -> (TempDir, FileStorage, GraphIndex) {
        let temp_dir = TempDir::new().unwrap();
        let storage = FileStorage::new(temp_dir.path().to_path_buf());
        let graph = GraphIndex::new();
        (temp_dir, storage, graph)
    }

    fn parse_response(result: &CallToolResult) -> TestResponse {
        let text = result.content[0]
            .raw
            .as_text()
            .expect("Expected text")
            .text
            .clone();
        serde_json::from_str(&text).expect("Expected valid JSON")
    }

    #[tokio::test]
    async fn test_read_existing_note() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        // Create a note
        fs::write(temp_dir.path().join("test.md"), "Hello, world!")
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        let result = execute(&storage, &graph, "test")
            .await
            .expect("should succeed");

        let response = parse_response(&result);
        assert_eq!(response.content, "Hello, world!");
        // Hash should be present and non-empty
        assert!(!response.content_hash.is_empty());
    }

    #[tokio::test]
    async fn test_read_returns_consistent_hash() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        let content = "Content";
        fs::write(temp_dir.path().join("test.md"), content)
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        // Read twice - should get same hash
        let result1 = execute(&storage, &graph, "test")
            .await
            .expect("should succeed");
        let result2 = execute(&storage, &graph, "test")
            .await
            .expect("should succeed");

        let response1 = parse_response(&result1);
        let response2 = parse_response(&result2);

        assert_eq!(response1.content_hash, response2.content_hash);
    }

    #[tokio::test]
    async fn test_read_note_in_subdirectory() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        // Create subdirectory and note
        fs::create_dir(temp_dir.path().join("knowledge"))
            .await
            .unwrap();
        fs::write(
            temp_dir.path().join("knowledge/My Note.md"),
            "Note content",
        )
        .await
        .unwrap();
        graph.update_note(
            "My Note",
            PathBuf::from("knowledge/My Note.md"),
            HashSet::new(),
        );

        let result = execute(&storage, &graph, "My Note")
            .await
            .expect("should succeed");

        let response = parse_response(&result);
        assert_eq!(response.content, "Note content");
    }

    #[tokio::test]
    async fn test_read_nonexistent_note_returns_error() {
        let (_temp_dir, storage, graph) = create_test_env().await;

        let result = execute(&storage, &graph, "nonexistent").await;

        // Should return an error, not success
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("Note not found"));
    }

    #[tokio::test]
    async fn test_read_with_wiki_link_syntax() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        fs::write(temp_dir.path().join("test.md"), "Content")
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        let result = execute(&storage, &graph, "[[test]]")
            .await
            .expect("should succeed");

        let response = parse_response(&result);
        assert_eq!(response.content, "Content");
    }

    #[tokio::test]
    async fn test_read_with_memory_uri() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        fs::create_dir(temp_dir.path().join("knowledge"))
            .await
            .unwrap();
        fs::write(temp_dir.path().join("knowledge/test.md"), "Content")
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("knowledge/test.md"), HashSet::new());

        let result = execute(&storage, &graph, "memory:knowledge/test")
            .await
            .expect("should succeed");

        let response = parse_response(&result);
        assert_eq!(response.content, "Content");
    }
}
