//! DeleteNote tool - delete a note from the vault.

use obsidian_fs::{ensure_markdown_extension, normalize_note_reference};
use rmcp::model::{CallToolResult, Content, ErrorData};
use std::path::Path;

use crate::storage::{Storage, StorageError};

/// Execute the DeleteNote tool.
///
/// Permanently deletes a note from the vault.
/// Returns an error if the note doesn't exist.
pub async fn execute<S: Storage>(
    vault_path: &Path,
    storage: &S,
    note: &str,
) -> Result<CallToolResult, ErrorData> {
    let normalized = normalize_note_reference(note);
    let uri = &normalized.path;

    let file_path = vault_path
        .join(ensure_markdown_extension(uri))
        .to_string_lossy()
        .to_string();

    // Delete the note
    storage.delete(uri).await.map_err(|e| match e {
        StorageError::NotFound { uri } => ErrorData::invalid_params(
            format!(
                "Note not found: {}. Cannot delete a note that doesn't exist.\n\
                 Expected file: {}",
                uri, file_path
            ),
            None,
        ),
        _ => ErrorData::internal_error(format!("Failed to delete note: {}", e), None),
    })?;

    let text = format!(
        "Deleted note: {}\n\n\
         **URI:** memory:{}\n\
         **File:** {}\n\n\
         The note has been permanently deleted.",
        normalized.name, uri, file_path
    );

    Ok(CallToolResult::success(vec![Content::text(text)]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::FileStorage;
    use tempfile::TempDir;
    use tokio::fs;

    async fn create_test_storage() -> (TempDir, FileStorage) {
        let temp_dir = TempDir::new().unwrap();
        let storage = FileStorage::new(temp_dir.path().to_path_buf());
        (temp_dir, storage)
    }

    #[tokio::test]
    async fn test_delete_existing_note() {
        let (temp_dir, storage) = create_test_storage().await;

        fs::write(temp_dir.path().join("test.md"), "Content")
            .await
            .unwrap();

        let result = execute(temp_dir.path(), &storage, "test")
            .await
            .expect("should succeed");

        let text = result.content[0]
            .raw
            .as_text()
            .expect("Expected text")
            .text
            .clone();

        assert!(text.contains("Deleted note"));
        assert!(text.contains("memory:test"));

        // Verify file was deleted
        assert!(!temp_dir.path().join("test.md").exists());
    }

    #[tokio::test]
    async fn test_delete_note_in_subdirectory() {
        let (temp_dir, storage) = create_test_storage().await;

        fs::create_dir(temp_dir.path().join("knowledge"))
            .await
            .unwrap();
        fs::write(temp_dir.path().join("knowledge/test.md"), "Content")
            .await
            .unwrap();

        let result = execute(temp_dir.path(), &storage, "knowledge/test")
            .await
            .expect("should succeed");

        let text = result.content[0]
            .raw
            .as_text()
            .expect("Expected text")
            .text
            .clone();

        assert!(text.contains("Deleted note"));
        assert!(!temp_dir.path().join("knowledge/test.md").exists());
    }

    #[tokio::test]
    async fn test_delete_nonexistent_note() {
        let (temp_dir, storage) = create_test_storage().await;

        let result = execute(temp_dir.path(), &storage, "nonexistent").await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("Note not found"));
    }

    #[tokio::test]
    async fn test_delete_with_wiki_link_syntax() {
        let (temp_dir, storage) = create_test_storage().await;

        fs::write(temp_dir.path().join("test.md"), "Content")
            .await
            .unwrap();

        let result = execute(temp_dir.path(), &storage, "[[test]]")
            .await
            .expect("should succeed");

        let text = result.content[0]
            .raw
            .as_text()
            .expect("Expected text")
            .text
            .clone();

        assert!(text.contains("Deleted note"));
        assert!(!temp_dir.path().join("test.md").exists());
    }
}
