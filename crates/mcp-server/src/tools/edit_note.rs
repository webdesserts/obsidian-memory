//! EditNote tool - make surgical text replacements in a note.
//!
//! Based on the MCP filesystem server's edit_file implementation,
//! this tool uses oldText/newText pairs for precise edits.

use obsidian_fs::{ensure_markdown_extension, normalize_note_reference};
use rmcp::model::{CallToolResult, Content, ErrorData};
use std::path::{Path, PathBuf};
use tokio::sync::RwLock;

use super::common::resolve_note_uri;
use crate::graph::GraphIndex;
use crate::storage::{ClientId, ReadWhitelist, Storage};

/// A single edit operation.
#[derive(Debug, Clone)]
pub struct Edit {
    /// Text to search for - must match exactly
    pub old_text: String,
    /// Text to replace with
    pub new_text: String,
}

/// Apply edits to content, returning the modified content and a diff.
fn apply_edits(content: &str, edits: &[Edit]) -> Result<(String, String), String> {
    let mut modified = content.to_string();
    let mut changes = Vec::new();

    for edit in edits {
        if !modified.contains(&edit.old_text) {
            return Err(format!(
                "Could not find text to replace:\n{}",
                truncate_for_display(&edit.old_text, 100)
            ));
        }

        // Count occurrences
        let count = modified.matches(&edit.old_text).count();
        if count > 1 {
            return Err(format!(
                "Text appears {} times in note - edit would be ambiguous:\n{}",
                count,
                truncate_for_display(&edit.old_text, 100)
            ));
        }

        modified = modified.replacen(&edit.old_text, &edit.new_text, 1);
        changes.push(format!(
            "- Replaced:\n  {}\n  With:\n  {}",
            truncate_for_display(&edit.old_text, 60),
            truncate_for_display(&edit.new_text, 60)
        ));
    }

    let diff = if changes.is_empty() {
        "No changes made.".to_string()
    } else {
        changes.join("\n\n")
    };

    Ok((modified, diff))
}

/// Truncate a string for display, adding ellipsis if needed.
fn truncate_for_display(s: &str, max_len: usize) -> String {
    let trimmed = s.trim();
    if trimmed.len() <= max_len {
        // Replace newlines with visible markers
        trimmed.replace('\n', "\\n")
    } else {
        format!("{}...", &trimmed[..max_len].replace('\n', "\\n"))
    }
}

/// Execute the EditNote tool.
///
/// Makes surgical text replacements using oldText/newText pairs.
/// Each oldText must appear exactly once in the note.
/// Requires that ReadNote was called first.
pub async fn execute<S: Storage>(
    vault_path: &Path,
    storage: &S,
    graph: &GraphIndex,
    read_whitelist: &RwLock<ReadWhitelist>,
    client_id: ClientId,
    note: &str,
    edits: Vec<Edit>,
    dry_run: bool,
) -> Result<CallToolResult, ErrorData> {
    // Resolve the note reference using the same logic as read_note
    let (uri, exists) = resolve_note_uri(storage, graph, note).await.map_err(|e| {
        ErrorData::internal_error(format!("Failed to resolve note: {}", e), None)
    })?;

    if !exists {
        return Err(ErrorData::invalid_params(
            format!("Note not found: {}", note),
            None,
        ));
    }

    let normalized = normalize_note_reference(note);

    // Check whitelist (must read before edit)
    {
        let whitelist = read_whitelist.read().await;
        if !whitelist.can_write(&client_id, &PathBuf::from(&uri)) {
            return Err(ErrorData::invalid_params(
                format!(
                    "Must read note before editing: {}\n\n\
                     Use ReadNote first to see the current content, then retry EditNote.",
                    note
                ),
                None,
            ));
        }
    }

    // Read current content (note existence already verified by resolve_note_uri)
    let (content, _metadata) = storage.read(&uri).await.map_err(|e| {
        ErrorData::internal_error(format!("Failed to read note: {}", e), None)
    })?;

    // Apply edits
    let (modified, diff) = apply_edits(&content, &edits).map_err(|e| {
        ErrorData::invalid_params(format!("Edit failed: {}", e), None)
    })?;

    let file_path = vault_path
        .join(ensure_markdown_extension(&uri))
        .to_string_lossy()
        .to_string();

    if dry_run {
        let text = format!(
            "Dry run - changes would be made to: {}\n\n\
             **URI:** memory:{}\n\
             **File:** {}\n\n\
             ## Changes\n\n{}\n\n\
             Use dryRun: false to apply these changes.",
            normalized.name, uri, file_path, diff
        );
        return Ok(CallToolResult::success(vec![Content::text(text)]));
    }

    // Write the modified content
    storage.write(&uri, &modified, None).await.map_err(|e| {
        ErrorData::internal_error(format!("Failed to write note: {}", e), None)
    })?;

    // Keep whitelist valid after edit (client knows the content)
    {
        let mut whitelist = read_whitelist.write().await;
        whitelist.mark_read(client_id, PathBuf::from(&uri));
    }

    let text = format!(
        "Edited note: {}\n\n\
         **URI:** memory:{}\n\
         **File:** {}\n\n\
         ## Changes\n\n{}",
        normalized.name, &uri, file_path, diff
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

    async fn mark_readable(whitelist: &RwLock<ReadWhitelist>, uri: &str) {
        let mut wl = whitelist.write().await;
        wl.mark_read(ClientId::stdio(), PathBuf::from(uri));
    }

    #[tokio::test]
    async fn test_edit_requires_read_first() {
        let (temp_dir, storage, mut graph, whitelist) = create_test_env().await;

        fs::write(temp_dir.path().join("test.md"), "Hello, world!")
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        let edits = vec![Edit {
            old_text: "world".to_string(),
            new_text: "Rust".to_string(),
        }];

        // Should fail without reading first
        let result = execute(
            temp_dir.path(),
            &storage,
            &graph,
            &whitelist,
            ClientId::stdio(),
            "test",
            edits,
            false,
        )
        .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("Must read note before editing"));
    }

    #[tokio::test]
    async fn test_edit_single_replacement() {
        let (temp_dir, storage, mut graph, whitelist) = create_test_env().await;

        fs::write(temp_dir.path().join("test.md"), "Hello, world!")
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());
        mark_readable(&whitelist, "test").await;

        let edits = vec![Edit {
            old_text: "world".to_string(),
            new_text: "Rust".to_string(),
        }];

        let result = execute(
            temp_dir.path(),
            &storage,
            &graph,
            &whitelist,
            ClientId::stdio(),
            "test",
            edits,
            false,
        )
        .await
        .expect("should succeed");

        let text = result.content[0]
            .raw
            .as_text()
            .expect("Expected text")
            .text
            .clone();

        assert!(text.contains("Edited note"));
        // Hash should NOT be exposed
        assert!(!text.contains("hash"));

        // Verify content changed
        let content = fs::read_to_string(temp_dir.path().join("test.md"))
            .await
            .unwrap();
        assert_eq!(content, "Hello, Rust!");
    }

    #[tokio::test]
    async fn test_edit_multiple_replacements() {
        let (temp_dir, storage, mut graph, whitelist) = create_test_env().await;

        fs::write(
            temp_dir.path().join("test.md"),
            "Hello, world! Goodbye, world!",
        )
        .await
        .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());
        mark_readable(&whitelist, "test").await;

        let edits = vec![
            Edit {
                old_text: "Hello".to_string(),
                new_text: "Hi".to_string(),
            },
            Edit {
                old_text: "Goodbye".to_string(),
                new_text: "Bye".to_string(),
            },
        ];

        let result = execute(
            temp_dir.path(),
            &storage,
            &graph,
            &whitelist,
            ClientId::stdio(),
            "test",
            edits,
            false,
        )
        .await
        .expect("should succeed");

        let text = result.content[0]
            .raw
            .as_text()
            .expect("Expected text")
            .text
            .clone();

        assert!(text.contains("Edited note"));

        // Verify content changed
        let content = fs::read_to_string(temp_dir.path().join("test.md"))
            .await
            .unwrap();
        assert_eq!(content, "Hi, world! Bye, world!");
    }

    #[tokio::test]
    async fn test_edit_fails_if_text_not_found() {
        let (temp_dir, storage, mut graph, whitelist) = create_test_env().await;

        fs::write(temp_dir.path().join("test.md"), "Hello, world!")
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());
        mark_readable(&whitelist, "test").await;

        let edits = vec![Edit {
            old_text: "nonexistent".to_string(),
            new_text: "replacement".to_string(),
        }];

        let result = execute(
            temp_dir.path(),
            &storage,
            &graph,
            &whitelist,
            ClientId::stdio(),
            "test",
            edits,
            false,
        )
        .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("Could not find text"));
    }

    #[tokio::test]
    async fn test_edit_fails_if_text_ambiguous() {
        let (temp_dir, storage, mut graph, whitelist) = create_test_env().await;

        fs::write(temp_dir.path().join("test.md"), "foo bar foo")
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());
        mark_readable(&whitelist, "test").await;

        let edits = vec![Edit {
            old_text: "foo".to_string(),
            new_text: "baz".to_string(),
        }];

        let result = execute(
            temp_dir.path(),
            &storage,
            &graph,
            &whitelist,
            ClientId::stdio(),
            "test",
            edits,
            false,
        )
        .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("appears 2 times"));
    }

    #[tokio::test]
    async fn test_edit_dry_run() {
        let (temp_dir, storage, mut graph, whitelist) = create_test_env().await;

        fs::write(temp_dir.path().join("test.md"), "Hello, world!")
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());
        mark_readable(&whitelist, "test").await;

        let edits = vec![Edit {
            old_text: "world".to_string(),
            new_text: "Rust".to_string(),
        }];

        let result = execute(
            temp_dir.path(),
            &storage,
            &graph,
            &whitelist,
            ClientId::stdio(),
            "test",
            edits,
            true,
        )
        .await
        .expect("should succeed");

        let text = result.content[0]
            .raw
            .as_text()
            .expect("Expected text")
            .text
            .clone();

        assert!(text.contains("Dry run"));
        assert!(text.contains("Replaced"));

        // Verify content was NOT changed
        let content = fs::read_to_string(temp_dir.path().join("test.md"))
            .await
            .unwrap();
        assert_eq!(content, "Hello, world!");
    }

    #[tokio::test]
    async fn test_edit_nonexistent_note() {
        let (temp_dir, storage, graph, whitelist) = create_test_env().await;
        mark_readable(&whitelist, "nonexistent").await;

        let edits = vec![Edit {
            old_text: "foo".to_string(),
            new_text: "bar".to_string(),
        }];

        let result = execute(
            temp_dir.path(),
            &storage,
            &graph,
            &whitelist,
            ClientId::stdio(),
            "nonexistent",
            edits,
            false,
        )
        .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("Note not found"));
    }

    // Integration tests - test the actual ReadNote→EditNote flow

    #[tokio::test]
    async fn test_read_then_edit_subdirectory_note_by_name() {
        // This is the bug scenario: note in subdirectory, referenced by name only
        let (temp_dir, storage, mut graph, whitelist) = create_test_env().await;

        // Create note in subdirectory
        fs::create_dir(temp_dir.path().join("knowledge")).await.unwrap();
        fs::write(
            temp_dir.path().join("knowledge/My Note.md"),
            "Hello, world!",
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
        assert_eq!(text, "Hello, world!");

        // Step 2: EditNote by name only (should use same resolution as ReadNote)
        let edits = vec![Edit {
            old_text: "world".to_string(),
            new_text: "Rust".to_string(),
        }];

        let edit_result = execute(
            temp_dir.path(),
            &storage,
            &graph,
            &whitelist,
            client,
            "My Note", // Same name-only reference
            edits,
            false,
        )
        .await
        .expect("EditNote should succeed after ReadNote");

        let edit_text = edit_result.content[0]
            .raw
            .as_text()
            .expect("Expected text")
            .text
            .clone();
        assert!(edit_text.contains("Edited note"));

        // Verify the file was actually modified
        let content = fs::read_to_string(temp_dir.path().join("knowledge/My Note.md"))
            .await
            .unwrap();
        assert_eq!(content, "Hello, Rust!");
    }

    #[tokio::test]
    async fn test_read_then_edit_with_wiki_link_syntax() {
        let (temp_dir, storage, mut graph, whitelist) = create_test_env().await;

        // Create note in subdirectory
        fs::create_dir(temp_dir.path().join("projects")).await.unwrap();
        fs::write(
            temp_dir.path().join("projects/foo-bar.md"),
            "Original content",
        )
        .await
        .unwrap();
        graph.update_note(
            "foo-bar",
            PathBuf::from("projects/foo-bar.md"),
            HashSet::new(),
        );

        let client = ClientId::stdio();

        // ReadNote with wiki-link syntax
        super::super::read_note::execute(
            &storage,
            &graph,
            &whitelist,
            client.clone(),
            "[[foo-bar]]",
        )
        .await
        .expect("ReadNote should succeed");

        // EditNote with wiki-link syntax
        let edits = vec![Edit {
            old_text: "Original".to_string(),
            new_text: "Modified".to_string(),
        }];

        execute(
            temp_dir.path(),
            &storage,
            &graph,
            &whitelist,
            client,
            "[[foo-bar]]",
            edits,
            false,
        )
        .await
        .expect("EditNote should succeed with wiki-link syntax");

        let content = fs::read_to_string(temp_dir.path().join("projects/foo-bar.md"))
            .await
            .unwrap();
        assert_eq!(content, "Modified content");
    }
}
