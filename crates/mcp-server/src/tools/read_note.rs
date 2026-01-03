//! ReadNote tool - read note content and mark as readable for writes.

use rmcp::model::{CallToolResult, Content, ErrorData};
use std::path::PathBuf;
use tokio::sync::RwLock;

use super::common::resolve_note_uri;
use crate::graph::GraphIndex;
use crate::storage::{ClientId, ReadWhitelist, Storage, StorageError};

/// Execute the ReadNote tool.
///
/// Returns note content and marks the note as writable for this client.
pub async fn execute<S: Storage>(
    storage: &S,
    graph: &GraphIndex,
    read_whitelist: &RwLock<ReadWhitelist>,
    client_id: ClientId,
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

    // Mark this note as readable for subsequent writes
    {
        let mut whitelist = read_whitelist.write().await;
        whitelist.mark_read(client_id, PathBuf::from(&uri));
    }

    // Return just the content, like the filesystem MCP server
    Ok(CallToolResult::success(vec![Content::text(content)]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::FileStorage;
    use std::collections::HashSet;
    use tempfile::TempDir;
    use tokio::fs;

    async fn create_test_env() -> (TempDir, FileStorage, GraphIndex, RwLock<ReadWhitelist>) {
        let temp_dir = TempDir::new().unwrap();
        let storage = FileStorage::new(temp_dir.path().to_path_buf());
        let graph = GraphIndex::new();
        let whitelist = RwLock::new(ReadWhitelist::new());
        (temp_dir, storage, graph, whitelist)
    }

    #[tokio::test]
    async fn test_read_existing_note() {
        let (temp_dir, storage, mut graph, whitelist) = create_test_env().await;

        // Create a note
        fs::write(temp_dir.path().join("test.md"), "Hello, world!")
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        let result = execute(&storage, &graph, &whitelist, ClientId::stdio(), "test")
            .await
            .expect("should succeed");

        let text = result.content[0]
            .raw
            .as_text()
            .expect("Expected text")
            .text
            .clone();

        // Should return just the content
        assert_eq!(text, "Hello, world!");
    }

    #[tokio::test]
    async fn test_read_marks_whitelist() {
        let (temp_dir, storage, mut graph, whitelist) = create_test_env().await;

        fs::write(temp_dir.path().join("test.md"), "Content")
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        let client = ClientId::stdio();

        // Before read, should not be whitelisted
        {
            let wl = whitelist.read().await;
            assert!(!wl.can_write(&client, &PathBuf::from("test")));
        }

        // Read the note
        execute(&storage, &graph, &whitelist, client.clone(), "test")
            .await
            .expect("should succeed");

        // After read, should be whitelisted
        {
            let wl = whitelist.read().await;
            assert!(wl.can_write(&client, &PathBuf::from("test")));
        }
    }

    #[tokio::test]
    async fn test_read_note_in_subdirectory() {
        let (temp_dir, storage, mut graph, whitelist) = create_test_env().await;

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

        let result = execute(&storage, &graph, &whitelist, ClientId::stdio(), "My Note")
            .await
            .expect("should succeed");

        let text = result.content[0]
            .raw
            .as_text()
            .expect("Expected text")
            .text
            .clone();

        assert_eq!(text, "Note content");
    }

    #[tokio::test]
    async fn test_read_nonexistent_note_returns_error() {
        let (_temp_dir, storage, graph, whitelist) = create_test_env().await;

        let result = execute(&storage, &graph, &whitelist, ClientId::stdio(), "nonexistent").await;

        // Should return an error, not success
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("Note not found"));
    }

    #[tokio::test]
    async fn test_read_with_wiki_link_syntax() {
        let (temp_dir, storage, mut graph, whitelist) = create_test_env().await;

        fs::write(temp_dir.path().join("test.md"), "Content")
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        let result = execute(&storage, &graph, &whitelist, ClientId::stdio(), "[[test]]")
            .await
            .expect("should succeed");

        let text = result.content[0]
            .raw
            .as_text()
            .expect("Expected text")
            .text
            .clone();

        assert_eq!(text, "Content");
    }

    #[tokio::test]
    async fn test_read_with_memory_uri() {
        let (temp_dir, storage, mut graph, whitelist) = create_test_env().await;

        fs::create_dir(temp_dir.path().join("knowledge"))
            .await
            .unwrap();
        fs::write(temp_dir.path().join("knowledge/test.md"), "Content")
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("knowledge/test.md"), HashSet::new());

        let result = execute(
            &storage,
            &graph,
            &whitelist,
            ClientId::stdio(),
            "memory:knowledge/test",
        )
        .await
        .expect("should succeed");

        let text = result.content[0]
            .raw
            .as_text()
            .expect("Expected text")
            .text
            .clone();

        assert_eq!(text, "Content");
    }
}
