//! WriteNote tool - write note content with optimistic locking via content_hash.

use obsidian_fs::ensure_markdown_extension;
use rmcp::model::{CallToolResult, Content, ErrorData};
use serde::Serialize;
use std::path::Path;

use super::common::resolve_note_uri;
use crate::graph::GraphIndex;
use crate::storage::{ContentHash, Storage, StorageError};

/// Response from WriteNote tool.
#[derive(Serialize)]
pub struct WriteNoteResponse {
    /// The memory URI of the note
    pub uri: String,
    /// The file path relative to vault
    pub path: String,
    /// New content hash after write - use this for subsequent writes
    pub content_hash: String,
    /// Number of bytes written
    pub bytes_written: usize,
}

/// Execute the WriteNote tool.
///
/// Creates new notes or overwrites existing ones.
/// For existing notes, content_hash parameter should be provided (will be required in future).
pub async fn execute<S: Storage>(
    vault_path: &Path,
    storage: &S,
    graph: &GraphIndex,
    note: &str,
    content: &str,
    content_hash: Option<&str>,
) -> Result<CallToolResult, ErrorData> {
    // Resolve the note reference using the graph index
    let (uri, exists) = resolve_note_uri(storage, graph, note).await.map_err(|e| {
        ErrorData::internal_error(format!("Failed to resolve note: {}", e), None)
    })?;

    // Validate content_hash for existing files
    if exists {
        match content_hash {
            Some(hash) => {
                // Validate the provided hash matches current content
                let (current_content, _) = storage.read(&uri).await.map_err(|e| {
                    ErrorData::internal_error(format!("Failed to read note for hash check: {}", e), None)
                })?;
                let current_hash = ContentHash::from_content(&current_content);
                if current_hash.as_str() != hash {
                    return Err(ErrorData::invalid_params(
                        format!(
                            "Note modified since last read (expected hash: {}, actual: {}). \
                             Read note again to get current content and hash.",
                            hash,
                            current_hash.as_str()
                        ),
                        None,
                    ));
                }
            }
            None => {
                // No hash provided for existing file - require it
                return Err(ErrorData::invalid_params(
                    "Note already exists. Read it first to get content_hash, then include in write request.".to_string(),
                    None,
                ));
            }
        }
    } else if content_hash.is_some() {
        // Hash provided but file doesn't exist
        return Err(ErrorData::invalid_params(
            format!("Note does not exist: {}", note),
            None,
        ));
    }

    // Attempt to write (pass hash for optimistic locking on existing files)
    storage.write(&uri, content, content_hash).await.map_err(|e| match e {
        StorageError::ParentNotFound { uri, parent } => ErrorData::invalid_params(
            format!(
                "Parent directory doesn't exist for '{}': {}. \
                 Create the directory first or use a different path.",
                uri,
                parent.display()
            ),
            None,
        ),
        StorageError::HashMismatch { expected, actual, .. } => ErrorData::invalid_params(
            format!(
                "Note modified since last read (expected hash: {}, actual: {}). \
                 Read note again to get current content and hash.",
                expected, actual
            ),
            None,
        ),
        _ => ErrorData::internal_error(format!("Failed to write note: {}", e), None),
    })?;

    // Compute new content hash for response
    let new_hash = ContentHash::from_content(content);

    let file_path = ensure_markdown_extension(&uri);

    let response = WriteNoteResponse {
        uri: format!("memory:{}", uri),
        path: file_path.clone(),
        content_hash: new_hash.as_str().to_string(),
        bytes_written: content.len(),
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
        uri: String,
        path: String,
        content_hash: String,
        bytes_written: usize,
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
    async fn test_write_new_note() {
        let (temp_dir, storage, graph) = create_test_env().await;

        let result = execute(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            "Hello, world!",
            None, // No hash for new file
        )
        .await
        .expect("should succeed");

        let response = parse_response(&result);
        assert_eq!(response.uri, "memory:test");
        assert_eq!(response.path, "test.md");
        assert_eq!(response.bytes_written, 13);
        assert!(!response.content_hash.is_empty());

        // Verify file was created
        let content = fs::read_to_string(temp_dir.path().join("test.md"))
            .await
            .unwrap();
        assert_eq!(content, "Hello, world!");
    }

    #[tokio::test]
    async fn test_write_existing_requires_content_hash() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        // Create existing note
        fs::write(temp_dir.path().join("test.md"), "Existing content")
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        // Try to write without content_hash
        let result = execute(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            "New content",
            None,
        )
        .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("Note already exists"));
        assert!(err.message.contains("Read it first"));
    }

    #[tokio::test]
    async fn test_write_existing_with_correct_hash() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        // Create existing note
        let original_content = "Version 1";
        fs::write(temp_dir.path().join("test.md"), original_content)
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        // Get the correct hash
        let correct_hash = ContentHash::from_content(original_content);

        // Write with correct hash should succeed
        let result = execute(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            "Version 2",
            Some(correct_hash.as_str()),
        )
        .await
        .expect("should succeed");

        let response = parse_response(&result);
        assert_eq!(response.uri, "memory:test");

        // Verify content changed
        let content = fs::read_to_string(temp_dir.path().join("test.md"))
            .await
            .unwrap();
        assert_eq!(content, "Version 2");
    }

    #[tokio::test]
    async fn test_write_existing_with_wrong_hash() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        // Create existing note
        fs::write(temp_dir.path().join("test.md"), "Existing content")
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        // Try to write with wrong hash
        let result = execute(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            "New content",
            Some("wrong_hash"),
        )
        .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("Note modified since last read"));
    }

    #[tokio::test]
    async fn test_write_returns_new_hash_for_chained_writes() {
        let (temp_dir, storage, graph) = create_test_env().await;

        // First write - creates new file
        let result1 = execute(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            "Version 1",
            None,
        )
        .await
        .expect("should succeed");

        let response1 = parse_response(&result1);

        // Second write - uses hash from first write
        let result2 = execute(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            "Version 2",
            Some(&response1.content_hash),
        )
        .await
        .expect("should succeed");

        let response2 = parse_response(&result2);

        // Hashes should be different
        assert_ne!(response1.content_hash, response2.content_hash);

        // Third write - uses hash from second write
        let result3 = execute(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            "Version 3",
            Some(&response2.content_hash),
        )
        .await
        .expect("should succeed");

        let response3 = parse_response(&result3);
        assert_ne!(response2.content_hash, response3.content_hash);

        // Verify final content
        let content = fs::read_to_string(temp_dir.path().join("test.md"))
            .await
            .unwrap();
        assert_eq!(content, "Version 3");
    }

    #[tokio::test]
    async fn test_write_to_subdirectory() {
        let (temp_dir, storage, graph) = create_test_env().await;

        // Create subdirectory
        fs::create_dir(temp_dir.path().join("knowledge"))
            .await
            .unwrap();

        let result = execute(
            temp_dir.path(),
            &storage,
            &graph,
            "knowledge/test",
            "Content",
            None,
        )
        .await
        .expect("should succeed");

        let response = parse_response(&result);
        assert_eq!(response.uri, "memory:knowledge/test");
        assert_eq!(response.path, "knowledge/test.md");

        // Verify file was created
        assert!(temp_dir.path().join("knowledge/test.md").exists());
    }

    #[tokio::test]
    async fn test_write_fails_if_parent_missing() {
        let (temp_dir, storage, graph) = create_test_env().await;

        let result = execute(
            temp_dir.path(),
            &storage,
            &graph,
            "missing/parent/test",
            "Content",
            None,
        )
        .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("Parent directory doesn't exist"));
    }

    #[tokio::test]
    async fn test_write_with_hash_for_nonexistent_file_fails() {
        let (temp_dir, storage, graph) = create_test_env().await;

        // Try to write with a hash when file doesn't exist
        let result = execute(
            temp_dir.path(),
            &storage,
            &graph,
            "new_note",
            "Content",
            Some("some_hash"),
        )
        .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("Note does not exist"));
    }

    // Integration tests - test the actual ReadNote→WriteNote flow

    #[tokio::test]
    async fn test_read_then_write_flow() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        // Create note in subdirectory
        fs::create_dir(temp_dir.path().join("knowledge")).await.unwrap();
        fs::write(
            temp_dir.path().join("knowledge/My Note.md"),
            "Version 1",
        )
        .await
        .unwrap();
        graph.update_note(
            "My Note",
            PathBuf::from("knowledge/My Note.md"),
            HashSet::new(),
        );

        // Step 1: ReadNote
        let read_result = super::super::read_note::execute(
            &storage,
            &graph,
            "My Note",
        )
        .await
        .expect("ReadNote should succeed");

        let read_json: serde_json::Value = serde_json::from_str(
            &read_result.content[0].raw.as_text().unwrap().text
        ).unwrap();

        let content_hash = read_json["content_hash"].as_str().unwrap();
        assert_eq!(read_json["content"].as_str().unwrap(), "Version 1");

        // Step 2: WriteNote with hash from read
        let write_result = execute(
            temp_dir.path(),
            &storage,
            &graph,
            "My Note",
            "Version 2",
            Some(content_hash),
        )
        .await
        .expect("WriteNote should succeed");

        let response = parse_response(&write_result);
        assert_eq!(response.uri, "memory:knowledge/My Note");

        // Verify the file was actually modified
        let content = fs::read_to_string(temp_dir.path().join("knowledge/My Note.md"))
            .await
            .unwrap();
        assert_eq!(content, "Version 2");
    }
}
