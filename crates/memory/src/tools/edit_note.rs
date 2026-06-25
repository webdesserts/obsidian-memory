//! EditNote tool - line-range based editing for notes.
//!
//! Replaces ranges of lines by line number, complementing the find-and-replace
//! approach in replace_in_note. Line numbers match the output of read_note.

use obsidian_fs::ensure_markdown_extension;
use rmcp::model::{CallToolResult, Content, ErrorData};
use serde::Serialize;
use std::path::Path;

use super::common::resolve_note_uri;
use crate::graph::GraphIndex;
use crate::storage::{ContentHash, Storage, StorageError};

/// A single line-range edit operation.
#[derive(Debug, Clone)]
pub struct LineEdit {
    /// First line to replace (1-indexed, inclusive)
    pub start_line: usize,
    /// Last line to replace (1-indexed, inclusive)
    pub end_line: usize,
    /// Replacement text (may contain newlines for multi-line replacement)
    pub new_text: String,
}

/// Response from EditNote tool.
#[derive(Serialize)]
pub struct EditNoteResponse {
    /// The memory URI of the note
    pub uri: String,
    /// The file path relative to vault
    pub path: String,
    /// New content hash after edit - use this for subsequent edits
    pub content_hash: String,
    /// Number of edits applied
    pub edits_applied: usize,
}

/// Response from EditNote dry run.
#[derive(Serialize)]
pub struct EditNoteDryRunResponse {
    /// The memory URI of the note
    pub uri: String,
    /// The file path relative to vault
    pub path: String,
    /// Hash that would result from applying edits
    pub would_produce_hash: String,
    /// Number of edits that would be applied
    pub edits_count: usize,
    /// Description of changes
    pub changes: String,
}

/// Apply line-range edits to content, returning modified content and a diff summary.
///
/// Edits are sorted by start_line descending and applied in reverse order so that
/// earlier line numbers remain valid as later ranges are replaced.
fn apply_line_edits(content: &str, edits: &[LineEdit]) -> Result<(String, String), String> {
    if edits.is_empty() {
        return Ok((content.to_string(), "No changes made.".to_string()));
    }

    let mut lines: Vec<&str> = content.split('\n').collect();
    let total_lines = lines.len();

    // Validate all edits before applying any
    for edit in edits {
        if edit.start_line == 0 {
            return Err("Line numbers are 1-indexed, got start_line=0".to_string());
        }
        if edit.end_line == 0 {
            return Err("Line numbers are 1-indexed, got end_line=0".to_string());
        }
        if edit.start_line > edit.end_line {
            return Err(format!(
                "start_line ({}) must be <= end_line ({})",
                edit.start_line, edit.end_line
            ));
        }
        if edit.end_line > total_lines {
            return Err(format!(
                "end_line ({}) exceeds total lines ({})",
                edit.end_line, total_lines
            ));
        }
    }

    // Sort by start_line to check for overlaps, then reverse for application
    let mut sorted: Vec<&LineEdit> = edits.iter().collect();
    sorted.sort_by_key(|e| e.start_line);

    // Check for overlapping ranges
    for pair in sorted.windows(2) {
        if pair[0].end_line >= pair[1].start_line {
            return Err(format!(
                "Overlapping ranges: lines {}-{} and {}-{}",
                pair[0].start_line, pair[0].end_line, pair[1].start_line, pair[1].end_line
            ));
        }
    }

    // Apply in reverse order (highest line numbers first) to preserve positions
    sorted.reverse();

    let mut changes = Vec::new();
    for edit in &sorted {
        // Convert from 1-indexed to 0-indexed
        let start = edit.start_line - 1;
        let end = edit.end_line; // exclusive upper bound for splice

        let old_text: String = lines[start..end].join("\n");

        // Split replacement into lines (empty string = delete the range)
        let replacement_lines: Vec<&str> = if edit.new_text.is_empty() {
            Vec::new()
        } else {
            edit.new_text.split('\n').collect()
        };

        lines.splice(start..end, replacement_lines);

        let action = if edit.new_text.is_empty() {
            format!("- Deleted lines {}-{}", edit.start_line, edit.end_line)
        } else {
            format!(
                "- Replaced lines {}-{}:\n  Was: {}\n  Now: {}",
                edit.start_line,
                edit.end_line,
                truncate_for_display(&old_text, 60),
                truncate_for_display(&edit.new_text, 60)
            )
        };
        changes.push(action);
    }

    // Reverse changes back to ascending order for readable output
    changes.reverse();

    Ok((lines.join("\n"), changes.join("\n\n")))
}

/// Truncate a string for display, adding ellipsis if needed.
fn truncate_for_display(s: &str, max_len: usize) -> String {
    let trimmed = s.trim();
    let char_count = trimmed.chars().count();
    if char_count <= max_len {
        trimmed.replace('\n', "\\n")
    } else {
        let truncated: String = trimmed.chars().take(max_len).collect();
        format!("{}...", truncated.replace('\n', "\\n"))
    }
}

/// Execute the EditNote tool.
///
/// Replaces ranges of lines by line number. Line numbers are 1-indexed and
/// inclusive on both ends, matching the output of read_note.
/// Requires content_hash from a previous ReadNote call.
pub async fn execute<S: Storage>(
    _vault_path: &Path,
    storage: &S,
    graph: &GraphIndex,
    note: &str,
    edits: Vec<LineEdit>,
    content_hash: &str,
    dry_run: bool,
) -> Result<CallToolResult, ErrorData> {
    let (uri, exists) = resolve_note_uri(storage, graph, note)
        .await
        .map_err(|e| ErrorData::internal_error(format!("Failed to resolve note: {}", e), None))?;

    if !exists {
        return Err(ErrorData::invalid_params(
            format!("Note not found: {}", note),
            None,
        ));
    }

    let (content, _metadata) = storage
        .read(&uri)
        .await
        .map_err(|e| ErrorData::internal_error(format!("Failed to read note: {}", e), None))?;

    // Validate content_hash matches current content
    let current_hash = ContentHash::from_content(&content);
    if current_hash.as_str() != content_hash {
        return Err(ErrorData::invalid_params(
            "Note modified since last read. Read the note again to get the \
             current content and hash before retrying."
                .to_string(),
            None,
        ));
    }

    let (modified, diff) = apply_line_edits(&content, &edits)
        .map_err(|e| ErrorData::invalid_params(format!("Edit failed: {}", e), None))?;

    let file_path = ensure_markdown_extension(&uri);
    let new_hash = ContentHash::from_content(&modified);

    if dry_run {
        let response = EditNoteDryRunResponse {
            uri: format!("memory:{}", uri),
            path: file_path,
            would_produce_hash: new_hash.as_str().to_string(),
            edits_count: edits.len(),
            changes: diff,
        };
        let json = serde_json::to_string(&response).map_err(|e| {
            ErrorData::internal_error(format!("Failed to serialize response: {}", e), None)
        })?;
        return Ok(CallToolResult::success(vec![Content::text(json)]));
    }

    storage
        .write(&uri, &modified, Some(content_hash))
        .await
        .map_err(|e| match e {
            StorageError::HashMismatch { .. } => ErrorData::invalid_params(
                "Note modified since last read. Read the note again to get the \
                 current content and hash before retrying."
                    .to_string(),
                None,
            ),
            _ => ErrorData::internal_error(format!("Failed to write note: {}", e), None),
        })?;

    let response = EditNoteResponse {
        uri: format!("memory:{}", uri),
        path: file_path,
        content_hash: new_hash.as_str().to_string(),
        edits_applied: edits.len(),
    };

    let json = serde_json::to_string(&response).map_err(|e| {
        ErrorData::internal_error(format!("Failed to serialize response: {}", e), None)
    })?;

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
        #[allow(dead_code)]
        path: String,
        content_hash: String,
        edits_applied: usize,
    }

    #[derive(Deserialize)]
    struct TestDryRunResponse {
        uri: String,
        would_produce_hash: String,
        edits_count: usize,
        #[allow(dead_code)]
        changes: String,
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

    fn parse_dry_run_response(result: &CallToolResult) -> TestDryRunResponse {
        let text = result.content[0]
            .raw
            .as_text()
            .expect("Expected text")
            .text
            .clone();
        serde_json::from_str(&text).expect("Expected valid JSON")
    }

    #[tokio::test]
    async fn test_single_range_replacement() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        let content = "line one\nline two\nline three";
        fs::write(temp_dir.path().join("test.md"), content)
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        let hash = ContentHash::from_content(content);
        let edits = vec![LineEdit {
            start_line: 2,
            end_line: 2,
            new_text: "replaced".to_string(),
        }];

        let result = execute(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            edits,
            hash.as_str(),
            false,
        )
        .await
        .expect("should succeed");

        let response = parse_response(&result);
        assert_eq!(response.edits_applied, 1);

        let written = fs::read_to_string(temp_dir.path().join("test.md"))
            .await
            .unwrap();
        assert_eq!(written, "line one\nreplaced\nline three");
    }

    #[tokio::test]
    async fn test_delete_lines() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        let content = "keep\ndelete me\nalso delete\nkeep too";
        fs::write(temp_dir.path().join("test.md"), content)
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        let hash = ContentHash::from_content(content);
        let edits = vec![LineEdit {
            start_line: 2,
            end_line: 3,
            new_text: String::new(),
        }];

        let result = execute(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            edits,
            hash.as_str(),
            false,
        )
        .await
        .expect("should succeed");

        let _ = parse_response(&result);
        let written = fs::read_to_string(temp_dir.path().join("test.md"))
            .await
            .unwrap();
        assert_eq!(written, "keep\nkeep too");
    }

    #[tokio::test]
    async fn test_replace_one_line_with_multiple() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        let content = "before\nold line\nafter";
        fs::write(temp_dir.path().join("test.md"), content)
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        let hash = ContentHash::from_content(content);
        let edits = vec![LineEdit {
            start_line: 2,
            end_line: 2,
            new_text: "new line A\nnew line B\nnew line C".to_string(),
        }];

        let result = execute(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            edits,
            hash.as_str(),
            false,
        )
        .await
        .expect("should succeed");

        let _ = parse_response(&result);
        let written = fs::read_to_string(temp_dir.path().join("test.md"))
            .await
            .unwrap();
        assert_eq!(written, "before\nnew line A\nnew line B\nnew line C\nafter");
    }

    #[tokio::test]
    async fn test_out_of_bounds_end_line() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        let content = "only one line";
        fs::write(temp_dir.path().join("test.md"), content)
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        let hash = ContentHash::from_content(content);
        let edits = vec![LineEdit {
            start_line: 1,
            end_line: 5,
            new_text: "replacement".to_string(),
        }];

        let result = execute(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            edits,
            hash.as_str(),
            false,
        )
        .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("exceeds total lines"));
    }

    #[tokio::test]
    async fn test_zero_indexed_line_error() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        let content = "line one";
        fs::write(temp_dir.path().join("test.md"), content)
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        let hash = ContentHash::from_content(content);
        let edits = vec![LineEdit {
            start_line: 0,
            end_line: 1,
            new_text: "replacement".to_string(),
        }];

        let result = execute(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            edits,
            hash.as_str(),
            false,
        )
        .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("1-indexed"));
    }

    #[tokio::test]
    async fn test_start_greater_than_end_error() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        let content = "line one\nline two";
        fs::write(temp_dir.path().join("test.md"), content)
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        let hash = ContentHash::from_content(content);
        let edits = vec![LineEdit {
            start_line: 2,
            end_line: 1,
            new_text: "replacement".to_string(),
        }];

        let result = execute(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            edits,
            hash.as_str(),
            false,
        )
        .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("must be <= end_line"));
    }

    #[tokio::test]
    async fn test_overlapping_ranges_error() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        let content = "a\nb\nc\nd";
        fs::write(temp_dir.path().join("test.md"), content)
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        let hash = ContentHash::from_content(content);
        let edits = vec![
            LineEdit {
                start_line: 1,
                end_line: 2,
                new_text: "x".to_string(),
            },
            LineEdit {
                start_line: 2,
                end_line: 3,
                new_text: "y".to_string(),
            },
        ];

        let result = execute(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            edits,
            hash.as_str(),
            false,
        )
        .await;
        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("Overlapping ranges"));
    }

    #[tokio::test]
    async fn test_multiple_non_overlapping_ranges() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        let content = "a\nb\nc\nd\ne";
        fs::write(temp_dir.path().join("test.md"), content)
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        let hash = ContentHash::from_content(content);
        // Provide edits out of order to test sorting
        let edits = vec![
            LineEdit {
                start_line: 4,
                end_line: 4,
                new_text: "D".to_string(),
            },
            LineEdit {
                start_line: 2,
                end_line: 2,
                new_text: "B".to_string(),
            },
        ];

        let result = execute(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            edits,
            hash.as_str(),
            false,
        )
        .await
        .expect("should succeed");

        let response = parse_response(&result);
        assert_eq!(response.edits_applied, 2);

        let written = fs::read_to_string(temp_dir.path().join("test.md"))
            .await
            .unwrap();
        assert_eq!(written, "a\nB\nc\nD\ne");
    }

    #[tokio::test]
    async fn test_dry_run_preview() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        let content = "line one\nline two\nline three";
        fs::write(temp_dir.path().join("test.md"), content)
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        let hash = ContentHash::from_content(content);
        let edits = vec![LineEdit {
            start_line: 2,
            end_line: 2,
            new_text: "replaced".to_string(),
        }];

        let result = execute(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            edits,
            hash.as_str(),
            true,
        )
        .await
        .expect("should succeed");

        let response = parse_dry_run_response(&result);
        assert_eq!(response.uri, "memory:test");
        assert!(!response.would_produce_hash.is_empty());
        assert_eq!(response.edits_count, 1);

        // Content should NOT have changed
        let written = fs::read_to_string(temp_dir.path().join("test.md"))
            .await
            .unwrap();
        assert_eq!(written, content);
    }

    #[tokio::test]
    async fn test_chained_edits_via_content_hash() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        let content = "a\nb\nc";
        fs::write(temp_dir.path().join("test.md"), content)
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        let hash = ContentHash::from_content(content);

        // First edit
        let edits1 = vec![LineEdit {
            start_line: 1,
            end_line: 1,
            new_text: "A".to_string(),
        }];
        let result1 = execute(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            edits1,
            hash.as_str(),
            false,
        )
        .await
        .expect("should succeed");
        let response1 = parse_response(&result1);

        // Second edit using hash from first
        let edits2 = vec![LineEdit {
            start_line: 3,
            end_line: 3,
            new_text: "C".to_string(),
        }];
        let result2 = execute(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            edits2,
            &response1.content_hash,
            false,
        )
        .await
        .expect("should succeed");
        let response2 = parse_response(&result2);

        assert_ne!(response1.content_hash, response2.content_hash);

        let written = fs::read_to_string(temp_dir.path().join("test.md"))
            .await
            .unwrap();
        assert_eq!(written, "A\nb\nC");
    }

    #[tokio::test]
    async fn test_trailing_newline_creates_phantom_line() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        // Most real files have a trailing newline, which split('\n') turns into an extra empty element
        let content = "line one\nline two\n";
        fs::write(temp_dir.path().join("test.md"), content)
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        let hash = ContentHash::from_content(content);

        // The trailing newline means split('\n') produces 3 elements: ["line one", "line two", ""]
        // So editing "line 2" should work and preserve the trailing newline
        let edits = vec![LineEdit {
            start_line: 2,
            end_line: 2,
            new_text: "replaced".to_string(),
        }];

        let result = execute(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            edits,
            hash.as_str(),
            false,
        )
        .await
        .expect("should succeed");

        let _ = parse_response(&result);
        let written = fs::read_to_string(temp_dir.path().join("test.md"))
            .await
            .unwrap();
        assert_eq!(written, "line one\nreplaced\n");
    }

    #[tokio::test]
    async fn test_read_then_edit_line_range_flow() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        // Create note in subdirectory (mirrors replace_in_note's integration test)
        fs::create_dir(temp_dir.path().join("knowledge"))
            .await
            .unwrap();
        fs::write(
            temp_dir.path().join("knowledge/My Note.md"),
            "first\nsecond\nthird",
        )
        .await
        .unwrap();
        graph.update_note(
            "My Note",
            PathBuf::from("knowledge/My Note.md"),
            HashSet::new(),
        );

        // Step 1: ReadNote to get content_hash
        let read_result = super::super::read_note::execute(&storage, &graph, "My Note")
            .await
            .expect("ReadNote should succeed");

        let read_json: serde_json::Value =
            serde_json::from_str(&read_result.content[0].raw.as_text().unwrap().text).unwrap();

        let content_hash = read_json["content_hash"].as_str().unwrap();
        // Verify line numbers appear in the content
        assert!(read_json["content"].as_str().unwrap().contains("1\tfirst"));

        // Step 2: EditNote with hash from read — replace line 2
        let edits = vec![LineEdit {
            start_line: 2,
            end_line: 2,
            new_text: "REPLACED".to_string(),
        }];

        let edit_result = execute(
            temp_dir.path(),
            &storage,
            &graph,
            "My Note",
            edits,
            content_hash,
            false,
        )
        .await
        .expect("EditNote should succeed");

        let response = parse_response(&edit_result);
        assert_eq!(response.uri, "memory:knowledge/My Note");

        // Verify the file was actually modified
        let content = fs::read_to_string(temp_dir.path().join("knowledge/My Note.md"))
            .await
            .unwrap();
        assert_eq!(content, "first\nREPLACED\nthird");
    }

    #[tokio::test]
    async fn test_unicode_content_does_not_panic() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        let content = "Hello 🌍\n日本語テスト\naccénted";
        fs::write(temp_dir.path().join("test.md"), content)
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        let hash = ContentHash::from_content(content);
        let edits = vec![LineEdit {
            start_line: 2,
            end_line: 2,
            new_text: "replaced".to_string(),
        }];

        let result = execute(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            edits,
            hash.as_str(),
            false,
        )
        .await
        .expect("should succeed");

        let _ = parse_response(&result);
        let written = fs::read_to_string(temp_dir.path().join("test.md"))
            .await
            .unwrap();
        assert_eq!(written, "Hello 🌍\nreplaced\naccénted");
    }
}
