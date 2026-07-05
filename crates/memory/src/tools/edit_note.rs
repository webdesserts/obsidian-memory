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
use crate::sections::write::{SectionWriteError, resolve_section_for_write, splice_section};
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

/// The generic "modified since last read" message shared by every hash
/// mismatch path (whole-note and, via the internal TOCTOU guard, section
/// writes too) - never echoes the current hash.
const MODIFIED_SINCE_READ: &str = "Note modified since last read. Read the note again to get the \
     current content and hash before retrying.";

/// Execute the EditNote tool for a whole-note edit.
///
/// Thin wrapper over [`execute_scoped`] with `section: None` - kept as a
/// distinct, unchanged entry point so every existing caller and test
/// continues to compile and pass without modification.
// kept: the MCP tool handler in main.rs calls execute_scoped directly, so
// this has no production caller, but every existing test (in this file and
// cross-file callers) still calls execute() unchanged, matching the
// read_note::execute precedent.
#[allow(dead_code)]
pub async fn execute<S: Storage>(
    vault_path: &Path,
    storage: &S,
    graph: &GraphIndex,
    note: &str,
    edits: Vec<LineEdit>,
    content_hash: &str,
    dry_run: bool,
) -> Result<CallToolResult, ErrorData> {
    execute_scoped(
        vault_path,
        storage,
        graph,
        note,
        edits,
        content_hash,
        dry_run,
        None,
    )
    .await
}

/// Execute the EditNote tool, optionally scoped to a single section.
///
/// When `section` is `None`, this is exactly today's whole-note logic: line
/// numbers in `edits` are absolute file lines and `content_hash` is the whole
/// file's hash.
///
/// When `section` is `Some(path)`, `edits`' line numbers are relative to the
/// section (line 1 = the section's own heading line) and `content_hash` is
/// the section's hash (from a prior section-scoped ReadNote). The section is
/// re-resolved fresh by path against the current file before editing, so a
/// shift in an earlier sibling section's size doesn't invalidate the target
/// section's hash - only a change to the target section's own content does.
#[allow(clippy::too_many_arguments)]
pub async fn execute_scoped<S: Storage>(
    _vault_path: &Path,
    storage: &S,
    graph: &GraphIndex,
    note: &str,
    edits: Vec<LineEdit>,
    content_hash: &str,
    dry_run: bool,
    section: Option<&str>,
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

    let Some(section_path) = section else {
        // Whole-note edit - this is exactly today's logic, reached either via
        // execute()'s wrapper or directly via execute_scoped(.., None).
        let current_hash = ContentHash::from_content(&content);
        if current_hash.as_str() != content_hash {
            return Err(ErrorData::invalid_params(
                MODIFIED_SINCE_READ.to_string(),
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
                StorageError::HashMismatch { .. } => {
                    ErrorData::invalid_params(MODIFIED_SINCE_READ.to_string(), None)
                }
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

        return Ok(CallToolResult::success(vec![Content::text(json)]));
    };

    // Section-scoped edit: resolve the section fresh against the just-read
    // content, verify its hash, edit the extracted slice with the existing
    // unmodified apply_line_edits (edits' line numbers are section-relative),
    // then splice the result back into the full file at the freshly resolved
    // absolute range.
    let resolved = resolve_section_for_write(&content, section_path, content_hash).map_err(
        |e| match e {
            SectionWriteError::NotFound { path } => ErrorData::invalid_params(
                format!(
                    "Section not found: {}. Use the outline tool to see available sections.",
                    path
                ),
                None,
            ),
            SectionWriteError::Ambiguous { path, candidates } => ErrorData::invalid_params(
                format!(
                    "Section path '{}' is ambiguous, matches: {}. Use a longer path to disambiguate.",
                    path,
                    candidates.join(", ")
                ),
                None,
            ),
            SectionWriteError::HashMismatch { .. } => {
                ErrorData::invalid_params(MODIFIED_SINCE_READ.to_string(), None)
            }
        },
    )?;

    let (modified_section, diff) = apply_line_edits(&resolved.section_content, &edits)
        .map_err(|e| ErrorData::invalid_params(format!("Edit failed: {}", e), None))?;

    let new_full_content = splice_section(
        &content,
        resolved.start_line,
        resolved.end_line,
        &modified_section,
    );
    let file_path = ensure_markdown_extension(&uri);
    let new_section_hash = ContentHash::from_content(&modified_section);

    if dry_run {
        let response = EditNoteDryRunResponse {
            uri: format!("memory:{}", uri),
            path: file_path,
            would_produce_hash: new_section_hash.as_str().to_string(),
            edits_count: edits.len(),
            changes: diff,
        };
        let json = serde_json::to_string(&response).map_err(|e| {
            ErrorData::internal_error(format!("Failed to serialize response: {}", e), None)
        })?;
        return Ok(CallToolResult::success(vec![Content::text(json)]));
    }

    // Whole-file hash captured from the same read used to resolve the
    // section, so a mismatch here can only mean a genuinely concurrent
    // out-of-band change to a different part of the file - the target
    // section's own drift was already caught above.
    let whole_file_hash = ContentHash::from_content(&content);
    storage
        .write(&uri, &new_full_content, Some(whole_file_hash.as_str()))
        .await
        .map_err(|e| match e {
            StorageError::HashMismatch { .. } => {
                ErrorData::invalid_params(MODIFIED_SINCE_READ.to_string(), None)
            }
            _ => ErrorData::internal_error(format!("Failed to write note: {}", e), None),
        })?;

    let response = EditNoteResponse {
        uri: format!("memory:{}", uri),
        path: file_path,
        content_hash: new_section_hash.as_str().to_string(),
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

    // -- Section-scoped edit tests (Commit 4) --------------------------------

    #[tokio::test]
    async fn test_section_edit_happy_path_relative_line_numbers() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        let content = "# Top\nintro\n## Middle\nmiddle line one\nmiddle line two\n# Top Two\nmore";
        fs::write(temp_dir.path().join("test.md"), content)
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        // Read the section first to get its hash, matching the intended usage.
        let section_content = "## Middle\nmiddle line one\nmiddle line two";
        let section_hash = ContentHash::from_content(section_content);

        // Line 1 of the section is its own heading line, so replacing line 2
        // targets "middle line one" without touching the heading or sibling
        // sections.
        let edits = vec![LineEdit {
            start_line: 2,
            end_line: 2,
            new_text: "REPLACED".to_string(),
        }];

        let result = execute_scoped(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            edits,
            section_hash.as_str(),
            false,
            Some("Top > Middle"),
        )
        .await
        .expect("should succeed");

        let response = parse_response(&result);
        let expected_new_section = "## Middle\nREPLACED\nmiddle line two";
        assert_eq!(
            response.content_hash,
            ContentHash::from_content(expected_new_section).as_str()
        );

        let written = fs::read_to_string(temp_dir.path().join("test.md"))
            .await
            .unwrap();
        assert_eq!(
            written,
            "# Top\nintro\n## Middle\nREPLACED\nmiddle line two\n# Top Two\nmore"
        );
    }

    #[tokio::test]
    async fn test_section_moved_relocated_by_path_at_write_time() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        let content = "# A\noriginal\n# B\ntarget";
        fs::write(temp_dir.path().join("test.md"), content)
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        // Read section B's hash before A grows.
        let section_b_hash = ContentHash::from_content("# B\ntarget");

        // Grow section A - this shifts B's absolute line numbers in the file.
        let grow_a_hash = ContentHash::from_content(content);
        execute_scoped(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            vec![LineEdit {
                start_line: 2,
                end_line: 2,
                new_text: "original\nextra line".to_string(),
            }],
            grow_a_hash.as_str(),
            false,
            None,
        )
        .await
        .expect("growing A should succeed");

        // B's content is unchanged, so its original hash still resolves - the
        // section is re-located by path, not by the old absolute range.
        let result = execute_scoped(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            vec![LineEdit {
                start_line: 2,
                end_line: 2,
                new_text: "changed".to_string(),
            }],
            section_b_hash.as_str(),
            false,
            Some("B"),
        )
        .await
        .expect("editing B by its original hash should succeed after A grew");
        let _ = parse_response(&result);

        let written = fs::read_to_string(temp_dir.path().join("test.md"))
            .await
            .unwrap();
        assert_eq!(written, "# A\noriginal\nextra line\n# B\nchanged");
    }

    #[tokio::test]
    async fn test_section_renamed_by_old_path_returns_not_found() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        let content = "# A\nbody a\n# B\nbody b";
        fs::write(temp_dir.path().join("test.md"), content)
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        // Rename "B" to "Renamed" via a separate whole-note write.
        let whole_hash = ContentHash::from_content(content);
        execute_scoped(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            vec![LineEdit {
                start_line: 3,
                end_line: 3,
                new_text: "# Renamed".to_string(),
            }],
            whole_hash.as_str(),
            false,
            None,
        )
        .await
        .expect("rename should succeed");

        // Editing by the old path fails cleanly, not-found - no crash.
        let result = execute_scoped(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            vec![LineEdit {
                start_line: 1,
                end_line: 1,
                new_text: "x".to_string(),
            }],
            "irrelevant_hash",
            false,
            Some("B"),
        )
        .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("not found"));
    }

    #[tokio::test]
    async fn test_section_content_changed_stale_hash_mismatch_redacted() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        let content = "# A\noriginal";
        fs::write(temp_dir.path().join("test.md"), content)
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        let stale_hash = ContentHash::from_content("# A\noriginal");

        // Someone else changes section A's content via a separate write.
        let whole_hash = ContentHash::from_content(content);
        let current_content_hash = execute_scoped(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            vec![LineEdit {
                start_line: 2,
                end_line: 2,
                new_text: "changed by someone else".to_string(),
            }],
            whole_hash.as_str(),
            false,
            None,
        )
        .await
        .expect("first write should succeed");
        let current_hash_str = parse_response(&current_content_hash).content_hash;

        // Now attempt an edit using the stale (pre-change) section hash.
        let result = execute_scoped(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            vec![LineEdit {
                start_line: 1,
                end_line: 1,
                new_text: "# A renamed".to_string(),
            }],
            stale_hash.as_str(),
            false,
            Some("A"),
        )
        .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("modified since last read"));
        // Positive redaction pin: the error must not leak the actual current
        // hash (whole-file or section) anywhere in its message.
        assert!(!err.message.contains(&current_hash_str));
        assert!(!err.message.contains(stale_hash.as_str()));
    }

    #[tokio::test]
    async fn test_section_edit_dry_run_disk_unchanged() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        let content = "# A\nbody a\n# B\nbody b";
        fs::write(temp_dir.path().join("test.md"), content)
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        let section_hash = ContentHash::from_content("# A\nbody a");

        let result = execute_scoped(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            vec![LineEdit {
                start_line: 2,
                end_line: 2,
                new_text: "would change".to_string(),
            }],
            section_hash.as_str(),
            true,
            Some("A"),
        )
        .await
        .expect("dry run should succeed");

        let response = parse_dry_run_response(&result);
        assert_eq!(
            response.would_produce_hash,
            ContentHash::from_content("# A\nwould change").as_str()
        );

        let written = fs::read_to_string(temp_dir.path().join("test.md"))
            .await
            .unwrap();
        assert_eq!(written, content, "dry run must not modify the file");
    }

    #[tokio::test]
    async fn test_section_edit_ambiguous_path_nothing_applied() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        let content = "# Notes\nfirst\n# Notes\nsecond";
        fs::write(temp_dir.path().join("test.md"), content)
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        let result = execute_scoped(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            vec![LineEdit {
                start_line: 1,
                end_line: 1,
                new_text: "x".to_string(),
            }],
            "irrelevant_hash",
            false,
            Some("Notes"),
        )
        .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("ambiguous"));

        // Verify nothing was applied via re-read.
        let written = fs::read_to_string(temp_dir.path().join("test.md"))
            .await
            .unwrap();
        assert_eq!(written, content);
    }

    #[tokio::test]
    async fn test_chained_section_edits_via_content_hash() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        let content = "# A\nfirst\nsecond";
        fs::write(temp_dir.path().join("test.md"), content)
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        let section_hash = ContentHash::from_content("# A\nfirst\nsecond");

        let result1 = execute_scoped(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            vec![LineEdit {
                start_line: 2,
                end_line: 2,
                new_text: "FIRST".to_string(),
            }],
            section_hash.as_str(),
            false,
            Some("A"),
        )
        .await
        .expect("first section edit should succeed");
        let response1 = parse_response(&result1);

        // Second edit uses the content_hash returned from the first edit.
        let result2 = execute_scoped(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            vec![LineEdit {
                start_line: 3,
                end_line: 3,
                new_text: "SECOND".to_string(),
            }],
            &response1.content_hash,
            false,
            Some("A"),
        )
        .await
        .expect("chained section edit should succeed");
        let _ = parse_response(&result2);

        let written = fs::read_to_string(temp_dir.path().join("test.md"))
            .await
            .unwrap();
        assert_eq!(written, "# A\nFIRST\nSECOND");
    }

    #[tokio::test]
    async fn test_oversized_note_section_edit_leaves_siblings_untouched() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        // A "big" note with several sections - the motivating oversized-note
        // scenario: read/edit just one small section without touching or
        // even transferring the rest of the file.
        let mut sections = Vec::new();
        for i in 1..=20 {
            sections.push(format!(
                "# Section {i}\nline one of {i}\nline two of {i}\nline three of {i}"
            ));
        }
        let content = sections.join("\n");
        fs::write(temp_dir.path().join("test.md"), &content)
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        let target_section_content =
            "# Section 10\nline one of 10\nline two of 10\nline three of 10";
        assert!(
            target_section_content.len() < content.len() / 4,
            "the extracted section should be much smaller than the whole file"
        );
        let section_hash = ContentHash::from_content(target_section_content);

        let result = execute_scoped(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            vec![LineEdit {
                start_line: 2,
                end_line: 2,
                new_text: "EDITED".to_string(),
            }],
            section_hash.as_str(),
            false,
            Some("Section 10"),
        )
        .await
        .expect("should succeed");
        let _ = parse_response(&result);

        let written = fs::read_to_string(temp_dir.path().join("test.md"))
            .await
            .unwrap();
        let expected = content.replace("line one of 10", "EDITED");
        assert_eq!(written, expected);

        // Every sibling section is byte-identical to before the edit.
        for i in [1, 2, 9, 11, 20] {
            let needle =
                format!("# Section {i}\nline one of {i}\nline two of {i}\nline three of {i}");
            assert!(written.contains(&needle), "section {i} should be untouched");
        }
    }

    #[tokio::test]
    async fn test_editing_frontmatter_to_remove_closing_delimiter_does_not_panic() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        let content = "---\ntitle: Test\n---\nbody";
        fs::write(temp_dir.path().join("test.md"), content)
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        let frontmatter_hash = ContentHash::from_content("---\ntitle: Test\n---");

        // Delete the closing delimiter line (the last line of the Frontmatter
        // section) - this doesn't panic, matching edit_note's existing risk
        // model of never validating resulting note structure.
        let result = execute_scoped(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            vec![LineEdit {
                start_line: 3,
                end_line: 3,
                new_text: String::new(),
            }],
            frontmatter_hash.as_str(),
            false,
            Some("Frontmatter"),
        )
        .await
        .expect("should not panic, even though it breaks frontmatter structure");
        let _ = parse_response(&result);

        // A subsequent outline shows zero Frontmatter sections.
        let written = fs::read_to_string(temp_dir.path().join("test.md"))
            .await
            .unwrap();
        let outline = crate::sections::outline::build_outline(&written);
        assert!(
            !outline
                .sections
                .iter()
                .any(|s| s.kind == crate::sections::outline::SectionKind::Frontmatter),
            "frontmatter section should no longer be recognized after its delimiter was removed"
        );
    }

    #[tokio::test]
    async fn test_edit_last_section_with_trailing_newline_preserves_trailing_byte() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        let content = "# A\nfirst\n# B\nlast\n";
        fs::write(temp_dir.path().join("test.md"), content)
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        // Section B's extracted content includes the phantom trailing line.
        let section_b_content = "# B\nlast\n";
        let section_hash = ContentHash::from_content(section_b_content);

        // A no-op edit (empty edit list) still round-trips through
        // resolve/splice - proving the trailing newline is preserved even
        // when nothing changes.
        let result = execute_scoped(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            vec![],
            section_hash.as_str(),
            false,
            Some("B"),
        )
        .await
        .expect("should succeed");
        let _ = parse_response(&result);

        let written = fs::read_to_string(temp_dir.path().join("test.md"))
            .await
            .unwrap();
        assert_eq!(written, content, "trailing newline must be preserved");
    }

    #[tokio::test]
    async fn test_edit_last_section_without_trailing_newline() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        let content = "# A\nfirst\n# B\nlast";
        fs::write(temp_dir.path().join("test.md"), content)
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        let section_hash = ContentHash::from_content("# B\nlast");

        let result = execute_scoped(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            vec![LineEdit {
                start_line: 2,
                end_line: 2,
                new_text: "changed".to_string(),
            }],
            section_hash.as_str(),
            false,
            Some("B"),
        )
        .await
        .expect("should succeed");
        let _ = parse_response(&result);

        let written = fs::read_to_string(temp_dir.path().join("test.md"))
            .await
            .unwrap();
        assert_eq!(written, "# A\nfirst\n# B\nchanged");
    }

    #[tokio::test]
    async fn test_empty_section_insert_body_text() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        // "A" is an empty section: heading immediately followed by another
        // heading of same-or-higher level.
        let content = "# A\n# B\nbody";
        fs::write(temp_dir.path().join("test.md"), content)
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        let section_hash = ContentHash::from_content("# A");

        // Insert a body line after the heading by replacing the single-line
        // section with heading + new content.
        let result = execute_scoped(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            vec![LineEdit {
                start_line: 1,
                end_line: 1,
                new_text: "# A\nnew body text".to_string(),
            }],
            section_hash.as_str(),
            false,
            Some("A"),
        )
        .await
        .expect("should succeed");
        let _ = parse_response(&result);

        let written = fs::read_to_string(temp_dir.path().join("test.md"))
            .await
            .unwrap();
        assert_eq!(written, "# A\nnew body text\n# B\nbody");
    }

    #[tokio::test]
    async fn test_heading_deletion_child_cascade_produces_clean_not_found() {
        let (temp_dir, storage, mut graph) = create_test_env().await;

        let content = "# Parent\nintro\n## Child\nchild body";
        fs::write(temp_dir.path().join("test.md"), content)
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        // Delete Parent's entire range, including its child, via a whole-note
        // edit spanning the full section (lines 1-4).
        let whole_hash = ContentHash::from_content(content);
        execute_scoped(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            vec![LineEdit {
                start_line: 1,
                end_line: 4,
                new_text: "just some text now".to_string(),
            }],
            whole_hash.as_str(),
            false,
            None,
        )
        .await
        .expect("deleting Parent's range should succeed");

        // An edit attempt on the former child, addressed by its old ancestor
        // path, cleanly 404s - re-locate-by-path semantics extend to
        // descendants of a deleted ancestor.
        let result = execute_scoped(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            vec![LineEdit {
                start_line: 1,
                end_line: 1,
                new_text: "x".to_string(),
            }],
            "irrelevant_hash",
            false,
            Some("Parent > Child"),
        )
        .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("not found"));
    }

    /// A `Storage` wrapper that forces a `HashMismatch` on the internal
    /// whole-file write, simulating a genuinely concurrent out-of-band change
    /// landing between resolve and write. Reads pass through unmodified.
    struct ForceWriteHashMismatchStorage<S> {
        inner: S,
    }

    #[async_trait::async_trait]
    impl<S: Storage> Storage for ForceWriteHashMismatchStorage<S> {
        async fn exists(&self, uri: &str) -> Result<bool, StorageError> {
            self.inner.exists(uri).await
        }

        async fn read(
            &self,
            uri: &str,
        ) -> Result<(String, crate::storage::NoteMetadata), StorageError> {
            self.inner.read(uri).await
        }

        async fn write(
            &self,
            uri: &str,
            _content: &str,
            _expected_hash: Option<&str>,
        ) -> Result<crate::storage::WriteResult, StorageError> {
            Err(StorageError::HashMismatch {
                uri: uri.to_string(),
            })
        }

        async fn delete(&self, uri: &str) -> Result<(), StorageError> {
            self.inner.delete(uri).await
        }

        async fn list(&self, prefix: &str) -> Result<Vec<String>, StorageError> {
            self.inner.list(prefix).await
        }

        async fn rename(&self, from: &str, to: &str) -> Result<(), StorageError> {
            self.inner.rename(from, to).await
        }
    }

    #[tokio::test]
    async fn test_internal_toctou_guard_maps_to_generic_redacted_message() {
        let temp_dir = TempDir::new().unwrap();
        let inner_storage = FileStorage::new(temp_dir.path().to_path_buf());
        let storage = ForceWriteHashMismatchStorage {
            inner: inner_storage,
        };
        let mut graph = GraphIndex::new();

        let content = "# A\nbody a\n# B\nbody b";
        fs::write(temp_dir.path().join("test.md"), content)
            .await
            .unwrap();
        graph.update_note("test", PathBuf::from("test.md"), HashSet::new());

        // The target section's own hash is correct - resolve_section_for_write
        // succeeds - but the wrapped storage forces the internal whole-file
        // write to fail with HashMismatch, simulating a concurrent change to
        // a *different* part of the file.
        let section_hash = ContentHash::from_content("# A\nbody a");

        let result = execute_scoped(
            temp_dir.path(),
            &storage,
            &graph,
            "test",
            vec![LineEdit {
                start_line: 2,
                end_line: 2,
                new_text: "changed".to_string(),
            }],
            section_hash.as_str(),
            false,
            Some("A"),
        )
        .await;

        assert!(result.is_err());
        let err = result.unwrap_err();
        assert!(err.message.contains("modified since last read"));
        // Still redacted - the generic wording, not a hash-bearing one.
        assert!(!err.message.to_lowercase().contains("hash:"));
    }
}
