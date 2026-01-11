//! MoveNote tool - move/rename a note and update backlinks.

use obsidian_fs::{ensure_markdown_extension, normalize_note_reference};
use rmcp::model::{CallToolResult, Content, ErrorData};
use std::path::Path;
use tokio::sync::RwLock;

use crate::graph::GraphIndex;
use crate::storage::{Storage, StorageError};

/// Execute the MoveNote tool.
///
/// Moves or renames a note and updates all notes that link to it.
/// Always updates backlinks automatically.
pub async fn execute<S: Storage>(
    vault_path: &Path,
    storage: &S,
    graph: &RwLock<GraphIndex>,
    from: &str,
    to: &str,
) -> Result<CallToolResult, ErrorData> {
    let from_normalized = normalize_note_reference(from);
    let to_normalized = normalize_note_reference(to);
    let from_uri = &from_normalized.path;
    let to_uri = &to_normalized.path;

    let from_file = vault_path
        .join(ensure_markdown_extension(from_uri))
        .to_string_lossy()
        .to_string();
    let to_file = vault_path
        .join(ensure_markdown_extension(to_uri))
        .to_string_lossy()
        .to_string();

    // Check source exists
    if !storage.exists(from_uri).await.map_err(|e| {
        ErrorData::internal_error(format!("Failed to check source: {}", e), None)
    })? {
        return Err(ErrorData::invalid_params(
            format!(
                "Source note not found: {}. Cannot move a note that doesn't exist.\n\
                 Expected file: {}",
                from, from_file
            ),
            None,
        ));
    }

    // Check destination doesn't exist
    if storage.exists(to_uri).await.map_err(|e| {
        ErrorData::internal_error(format!("Failed to check destination: {}", e), None)
    })? {
        return Err(ErrorData::invalid_params(
            format!(
                "Destination already exists: {}. Cannot overwrite an existing note.\n\
                 Existing file: {}",
                to, to_file
            ),
            None,
        ));
    }

    // Find and update backlinks before the move
    let mut backlinks_updated = Vec::new();
    {
        let graph_read = graph.read().await;

        // Get notes that link to the source note by name
        if let Some(linking_paths) = graph_read.get_backlinks(&from_normalized.name) {
            let old_link = format!("[[{}]]", from_normalized.name);
            let new_link = format!("[[{}]]", to_normalized.name);

            for path in linking_paths.iter() {
                // Skip the source note itself
                if path == &ensure_markdown_extension(from_uri) {
                    continue;
                }

                // Convert path to URI (remove .md)
                let uri = path.strip_suffix(".md").unwrap_or(path);

                // Read the linking note
                if let Ok((content, _)) = storage.read(uri).await {
                    // Replace the wiki-link
                    if content.contains(&old_link) {
                        let updated = content.replace(&old_link, &new_link);

                        // Write back
                        if storage.write(uri, &updated, None).await.is_ok() {
                            backlinks_updated.push(uri.to_string());
                        }
                    }
                }
            }
        }
    }

    // Perform the rename
    storage.rename(from_uri, to_uri).await.map_err(|e| match e {
        StorageError::ParentNotFound { uri, parent } => ErrorData::invalid_params(
            format!(
                "Parent directory doesn't exist for '{}': {}. \
                 Create the directory first.",
                uri,
                parent.display()
            ),
            None,
        ),
        _ => ErrorData::internal_error(format!("Failed to move note: {}", e), None),
    })?;

    // Build response
    let backlinks_summary = if !backlinks_updated.is_empty() {
        format!(
            "\n\n## Updated Backlinks\n\nUpdated {} note(s) that linked to this note:\n{}",
            backlinks_updated.len(),
            backlinks_updated
                .iter()
                .map(|p| format!("- memory:{}", p))
                .collect::<Vec<_>>()
                .join("\n")
        )
    } else {
        "\n\nNo backlinks to update.".to_string()
    };

    let text = format!(
        "Moved note: {} -> {}\n\n\
         **From:** memory:{}\n\
         **To:** memory:{}\n\
         **New file:** {}{}",
        from_normalized.name, to_normalized.name, from_uri, to_uri, to_file, backlinks_summary
    );

    Ok(CallToolResult::success(vec![Content::text(text)]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::storage::FileStorage;
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::sync::Arc;
    use tempfile::TempDir;
    use tokio::fs;

    async fn create_test_env() -> (TempDir, FileStorage, Arc<RwLock<GraphIndex>>) {
        let temp_dir = TempDir::new().unwrap();
        let storage = FileStorage::new(temp_dir.path().to_path_buf());
        let graph = Arc::new(RwLock::new(GraphIndex::new()));
        (temp_dir, storage, graph)
    }

    #[tokio::test]
    async fn test_move_simple_rename() {
        let (temp_dir, storage, graph) = create_test_env().await;

        fs::write(temp_dir.path().join("old.md"), "Content")
            .await
            .unwrap();

        let result = execute(temp_dir.path(), &storage, &graph, "old", "new")
            .await
            .expect("should succeed");

        let text = result.content[0]
            .raw
            .as_text()
            .expect("Expected text")
            .text
            .clone();

        assert!(text.contains("Moved note"));
        assert!(!temp_dir.path().join("old.md").exists());
        assert!(temp_dir.path().join("new.md").exists());
    }

    #[tokio::test]
    async fn test_move_between_directories() {
        let (temp_dir, storage, graph) = create_test_env().await;

        fs::write(temp_dir.path().join("test.md"), "Content")
            .await
            .unwrap();
        fs::create_dir(temp_dir.path().join("knowledge"))
            .await
            .unwrap();

        let result = execute(temp_dir.path(), &storage, &graph, "test", "knowledge/test")
            .await
            .expect("should succeed");

        let text = result.content[0]
            .raw
            .as_text()
            .expect("Expected text")
            .text
            .clone();

        assert!(text.contains("Moved note"));
        assert!(!temp_dir.path().join("test.md").exists());
        assert!(temp_dir.path().join("knowledge/test.md").exists());
    }

    #[tokio::test]
    async fn test_move_updates_backlinks() {
        let (temp_dir, storage, graph) = create_test_env().await;

        // Create note A that links to B
        fs::write(temp_dir.path().join("A.md"), "Link to [[B]]")
            .await
            .unwrap();
        fs::write(temp_dir.path().join("B.md"), "Target note")
            .await
            .unwrap();

        // Update graph with the link
        {
            let mut g = graph.write().await;
            g.update_note(
                "A",
                PathBuf::from("A.md"),
                ["B".to_string()].into_iter().collect(),
            );
            g.update_note("B", PathBuf::from("B.md"), HashSet::new());
        }

        let result = execute(temp_dir.path(), &storage, &graph, "B", "C")
            .await
            .expect("should succeed");

        let text = result.content[0]
            .raw
            .as_text()
            .expect("Expected text")
            .text
            .clone();

        assert!(text.contains("Updated Backlinks"));
        assert!(text.contains("memory:A"));

        // Verify A was updated
        let a_content = fs::read_to_string(temp_dir.path().join("A.md"))
            .await
            .unwrap();
        assert!(a_content.contains("[[C]]"));
        assert!(!a_content.contains("[[B]]"));
    }

    #[tokio::test]
    async fn test_move_source_not_found() {
        let (temp_dir, storage, graph) = create_test_env().await;

        let result = execute(temp_dir.path(), &storage, &graph, "nonexistent", "new").await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("Source note not found"));
    }

    #[tokio::test]
    async fn test_move_destination_exists() {
        let (temp_dir, storage, graph) = create_test_env().await;

        fs::write(temp_dir.path().join("source.md"), "Source")
            .await
            .unwrap();
        fs::write(temp_dir.path().join("dest.md"), "Dest")
            .await
            .unwrap();

        let result = execute(temp_dir.path(), &storage, &graph, "source", "dest").await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("Destination already exists"));
    }

    #[tokio::test]
    async fn test_move_parent_missing() {
        let (temp_dir, storage, graph) = create_test_env().await;

        fs::write(temp_dir.path().join("test.md"), "Content")
            .await
            .unwrap();

        let result = execute(temp_dir.path(), &storage, &graph, "test", "missing/dir/test").await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("Parent directory doesn't exist"));
    }
}
