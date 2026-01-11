//! WriteNote tool - write note content with "must read first" check for existing notes.

use obsidian_fs::{ensure_markdown_extension, normalize_note_reference};
use rmcp::model::{CallToolResult, Content, ErrorData};
use std::path::{Path, PathBuf};
use tokio::sync::RwLock;

use super::common::resolve_note_uri;
use crate::graph::GraphIndex;
use crate::storage::{ClientId, ContentHash, ReadWhitelist, Storage, StorageError};

/// Execute the WriteNote tool.
///
/// Creates new notes or overwrites existing ones.
/// For existing notes, requires that ReadNote was called first.
pub async fn execute<S: Storage>(
    vault_path: &Path,
    storage: &S,
    graph: &GraphIndex,
    read_whitelist: &RwLock<ReadWhitelist>,
    client_id: ClientId,
    note: &str,
    content: &str,
) -> Result<CallToolResult, ErrorData> {
    let normalized = normalize_note_reference(note);

    // Resolve the note reference using the graph index
    let (uri, exists) = resolve_note_uri(storage, graph, note).await.map_err(|e| {
        ErrorData::internal_error(format!("Failed to resolve note: {}", e), None)
    })?;

    // For existing notes, check whitelist using content hash (must read before write)
    // We store the hash to pass to storage.write() for optimistic locking (TOCTOU protection)
    let expected_hash = if exists {
        // Read current content to compute hash for comparison
        let (current_content, _) = storage.read(&uri).await.map_err(|e| {
            ErrorData::internal_error(format!("Failed to read note for hash check: {}", e), None)
        })?;
        let current_hash = ContentHash::from_content(&current_content);
        
        let whitelist = read_whitelist.read().await;
        if !whitelist.can_write(&client_id, &PathBuf::from(&uri), &current_hash) {
            return Err(ErrorData::invalid_params(
                format!(
                    "Must read note before writing: {}\n\n\
                     Use ReadNote first to see the current content, then retry WriteNote.\n\
                     (If you already read it, the file may have been modified externally.)",
                    note
                ),
                None,
            ));
        }
        Some(current_hash)
    } else {
        None
    };

    // Attempt to write with optimistic locking (pass hash for existing files)
    let _result = storage.write(&uri, content, expected_hash.as_ref().map(|h| h.as_str())).await.map_err(|e| match e {
        StorageError::ParentNotFound { uri, parent } => ErrorData::invalid_params(
            format!(
                "Parent directory doesn't exist for '{}': {}. \
                 Create the directory first or use a different path.",
                uri,
                parent.display()
            ),
            None,
        ),
        _ => ErrorData::internal_error(format!("Failed to write note: {}", e), None),
    })?;

    // After successful write, update whitelist with new content hash
    // (the client knows the content since they just wrote it)
    {
        let new_hash = ContentHash::from_content(content);
        let mut whitelist = read_whitelist.write().await;
        whitelist.mark_read(client_id, PathBuf::from(&uri), new_hash);
    }

    let file_path = vault_path
        .join(ensure_markdown_extension(&uri))
        .to_string_lossy()
        .to_string();

    let action = if exists { "Updated" } else { "Created" };

    let text = format!(
        "{} note: {}\n\n\
         **URI:** memory:{}\n\
         **File:** {}\n\n\
         Content written successfully ({} bytes).",
        action, normalized.name, &uri, file_path, content.len()
    );

    Ok(CallToolResult::success(vec![Content::text(text)]))
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
    async fn test_write_new_note() {
        let (temp_dir, storage, graph, whitelist) = create_test_env().await;

        let result = execute(
            temp_dir.path(),
            &storage,
            &graph,
            &whitelist,
            ClientId::stdio(),
            "test",
            "Hello, world!",
        )
        .await
        .expect("should succeed");

        let text = result.content[0]
            .raw
            .as_text()
            .expect("Expected text")
            .text
            .clone();

        assert!(text.contains("Created note"));
        assert!(text.contains("memory:test"));
        // Hash should NOT be exposed
        assert!(!text.contains("Hash:"));

        // Verify file was created
        let content = fs::read_to_string(temp_dir.path().join("test.md"))
            .await
            .unwrap();
        assert_eq!(content, "Hello, world!");
    }

    #[tokio::test]
    async fn test_write_existing_requires_read_first() {
        let (temp_dir, storage, mut graph, whitelist) = create_test_env().await;

        // Create existing note
        fs::write(temp_dir.path().join("test.md"), "Existing content")
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        // Try to write without reading first
        let result = execute(
            temp_dir.path(),
            &storage,
            &graph,
            &whitelist,
            ClientId::stdio(),
            "test",
            "New content",
        )
        .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("Must read note before writing"));
    }

    #[tokio::test]
    async fn test_write_existing_after_read() {
        let (temp_dir, storage, mut graph, whitelist) = create_test_env().await;

        // Create existing note
        let original_content = "Version 1";
        fs::write(temp_dir.path().join("test.md"), original_content)
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        let client = ClientId::stdio();

        // Mark as read (simulating ReadNote) - must include content hash
        {
            let hash = ContentHash::from_content(original_content);
            let mut wl = whitelist.write().await;
            wl.mark_read(client.clone(), PathBuf::from("test"), hash);
        }

        // Now write should succeed
        let result = execute(
            temp_dir.path(),
            &storage,
            &graph,
            &whitelist,
            client,
            "test",
            "Version 2",
        )
        .await
        .expect("should succeed");

        let text = result.content[0]
            .raw
            .as_text()
            .expect("Expected text")
            .text
            .clone();

        assert!(text.contains("Updated note"));

        // Verify content changed
        let content = fs::read_to_string(temp_dir.path().join("test.md"))
            .await
            .unwrap();
        assert_eq!(content, "Version 2");
    }

    #[tokio::test]
    async fn test_write_updates_whitelist() {
        let (temp_dir, storage, graph, whitelist) = create_test_env().await;
        let client = ClientId::stdio();

        let first_content = "Content";

        // Write a new note
        execute(
            temp_dir.path(),
            &storage,
            &graph,
            &whitelist,
            client.clone(),
            "test",
            first_content,
        )
        .await
        .expect("should succeed");

        // Should now be whitelisted with the new content's hash
        {
            let content_hash = ContentHash::from_content(first_content);
            let wl = whitelist.read().await;
            assert!(wl.can_write(&client, &PathBuf::from("test"), &content_hash));
        }

        // Second write should succeed without explicit read
        let result = execute(
            temp_dir.path(),
            &storage,
            &graph,
            &whitelist,
            client,
            "test",
            "Updated content",
        )
        .await;

        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn test_write_to_subdirectory() {
        let (temp_dir, storage, graph, whitelist) = create_test_env().await;

        // Create subdirectory
        fs::create_dir(temp_dir.path().join("knowledge"))
            .await
            .unwrap();

        let result = execute(
            temp_dir.path(),
            &storage,
            &graph,
            &whitelist,
            ClientId::stdio(),
            "knowledge/test",
            "Content",
        )
        .await
        .expect("should succeed");

        let text = result.content[0]
            .raw
            .as_text()
            .expect("Expected text")
            .text
            .clone();

        assert!(text.contains("Created note"));
        assert!(text.contains("memory:knowledge/test"));

        // Verify file was created
        assert!(temp_dir.path().join("knowledge/test.md").exists());
    }

    #[tokio::test]
    async fn test_write_fails_if_parent_missing() {
        let (temp_dir, storage, graph, whitelist) = create_test_env().await;

        let result = execute(
            temp_dir.path(),
            &storage,
            &graph,
            &whitelist,
            ClientId::stdio(),
            "missing/parent/test",
            "Content",
        )
        .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("Parent directory doesn't exist"));
    }

    #[tokio::test]
    async fn test_write_with_wiki_link_syntax() {
        let (temp_dir, storage, graph, whitelist) = create_test_env().await;

        let result = execute(
            temp_dir.path(),
            &storage,
            &graph,
            &whitelist,
            ClientId::stdio(),
            "[[test]]",
            "Content",
        )
        .await
        .expect("should succeed");

        let text = result.content[0]
            .raw
            .as_text()
            .expect("Expected text")
            .text
            .clone();

        assert!(text.contains("Created note"));
        assert!(temp_dir.path().join("test.md").exists());
    }

    // Integration tests - test the actual ReadNote→WriteNote flow

    #[tokio::test]
    async fn test_read_then_write_subdirectory_note_by_name() {
        // This tests the bug scenario: note in subdirectory, referenced by name only
        let (temp_dir, storage, mut graph, whitelist) = create_test_env().await;

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

        let client = ClientId::stdio();

        // Step 1: ReadNote by name only (not full path)
        let read_result = super::super::read_note::execute(
            &storage,
            &graph,
            &whitelist,
            client.clone(),
            "My Note", // Just the name, not "knowledge/My Note"
        )
        .await
        .expect("ReadNote should succeed");

        let text = read_result.content[0]
            .raw
            .as_text()
            .expect("Expected text")
            .text
            .clone();
        assert_eq!(text, "Version 1");

        // Step 2: WriteNote by name only (should use same resolution as ReadNote)
        let write_result = execute(
            temp_dir.path(),
            &storage,
            &graph,
            &whitelist,
            client,
            "My Note", // Same name-only reference
            "Version 2",
        )
        .await
        .expect("WriteNote should succeed after ReadNote");

        let write_text = write_result.content[0]
            .raw
            .as_text()
            .expect("Expected text")
            .text
            .clone();
        assert!(write_text.contains("Updated note"));

        // Verify the file was actually modified
        let content = fs::read_to_string(temp_dir.path().join("knowledge/My Note.md"))
            .await
            .unwrap();
        assert_eq!(content, "Version 2");
    }
}
