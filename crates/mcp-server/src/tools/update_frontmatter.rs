use obsidian_fs::{build_note_with_frontmatter, ensure_markdown_extension, parse_frontmatter, Frontmatter};
use rmcp::model::{CallToolResult, Content, ErrorData};
use serde_json::Value as JsonValue;
use std::collections::HashMap;
use std::path::Path;
use tokio::fs;

/// Update frontmatter in a note file.
///
/// Reads the existing note, merges the frontmatter updates, and writes back.
pub async fn execute(
    vault_path: &Path,
    relative_path: &str,
    updates: HashMap<String, JsonValue>,
) -> Result<CallToolResult, ErrorData> {
    let note_path = ensure_markdown_extension(relative_path);
    let absolute_path = vault_path.join(&note_path);

    // Validate path is within vault BEFORE reading (prevent directory traversal)
    let canonical_vault = vault_path
        .canonicalize()
        .map_err(|e| ErrorData::internal_error(format!("Invalid vault path: {}", e), None))?;

    // Canonicalize the target path and verify it's within vault
    let canonical_file = absolute_path
        .canonicalize()
        .map_err(|e| ErrorData::invalid_params(format!("Invalid path: {}", e), None))?;

    if !canonical_file.starts_with(&canonical_vault) {
        return Err(ErrorData::invalid_params(
            format!("Path outside vault: {}", relative_path),
            None,
        ));
    }

    // Read existing file (now safe - path validated)
    let raw_content = fs::read_to_string(&canonical_file)
        .await
        .map_err(|e| ErrorData::invalid_params(format!("Failed to read note: {}", e), None))?;

    // Parse existing frontmatter
    let parsed = parse_frontmatter(&raw_content);
    let existing_frontmatter = parsed.frontmatter.unwrap_or_default();
    let content = parsed.content;

    // Merge updates into existing frontmatter
    let mut merged: Frontmatter = existing_frontmatter;
    for (key, value) in updates {
        merged.insert(key, value);
    }

    // Rebuild file content with updated frontmatter
    let new_content = build_note_with_frontmatter(&merged, content)
        .map_err(|e| ErrorData::internal_error(e.to_string(), None))?;

    // Write back
    fs::write(&absolute_path, new_content)
        .await
        .map_err(|e| ErrorData::internal_error(format!("Failed to write note: {}", e), None))?;

    let text = format!("Frontmatter updated: {}", note_path);
    Ok(CallToolResult::success(vec![Content::text(text)]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn create_test_note(dir: &Path, name: &str, content: &str) {
        let path = dir.join(name);
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent).await.unwrap();
        }
        fs::write(path, content).await.unwrap();
    }

    #[tokio::test]
    async fn test_update_existing_frontmatter() {
        let temp_dir = TempDir::new().unwrap();
        let vault = temp_dir.path();

        let initial_content = "---\ntype: note\ntags:\n  - one\n---\n\nContent here";
        create_test_note(vault, "test.md", initial_content).await;

        let mut updates = HashMap::new();
        updates.insert("type".to_string(), JsonValue::String("updated".to_string()));
        updates.insert("new_field".to_string(), JsonValue::Bool(true));

        let result = execute(vault, "test", updates).await.expect("should succeed");
        assert!(result.content[0].raw.as_text().unwrap().text.contains("Frontmatter updated"));

        // Verify the file was updated
        let updated = fs::read_to_string(vault.join("test.md")).await.unwrap();
        assert!(updated.contains("type: updated"));
        assert!(updated.contains("new_field: true"));
        // Original field should still be there
        assert!(updated.contains("tags:"));
    }

    #[tokio::test]
    async fn test_add_frontmatter_to_note_without_frontmatter() {
        let temp_dir = TempDir::new().unwrap();
        let vault = temp_dir.path();

        let initial_content = "Just content, no frontmatter";
        create_test_note(vault, "test.md", initial_content).await;

        let mut updates = HashMap::new();
        updates.insert("type".to_string(), JsonValue::String("new".to_string()));

        execute(vault, "test", updates).await.expect("should succeed");

        let updated = fs::read_to_string(vault.join("test.md")).await.unwrap();
        assert!(updated.starts_with("---\n"));
        assert!(updated.contains("type: new"));
        assert!(updated.contains("Just content, no frontmatter"));
    }

    #[tokio::test]
    async fn test_update_note_in_subfolder() {
        let temp_dir = TempDir::new().unwrap();
        let vault = temp_dir.path();

        let initial_content = "---\ntype: project\n---\n\nProject content";
        create_test_note(vault, "projects/MyProject.md", initial_content).await;

        let mut updates = HashMap::new();
        updates.insert("status".to_string(), JsonValue::String("active".to_string()));

        execute(vault, "projects/MyProject", updates).await.expect("should succeed");

        let updated = fs::read_to_string(vault.join("projects/MyProject.md")).await.unwrap();
        assert!(updated.contains("status: active"));
        assert!(updated.contains("type: project"));
    }

    #[tokio::test]
    async fn test_nonexistent_file_returns_error() {
        let temp_dir = TempDir::new().unwrap();
        let vault = temp_dir.path();

        let mut updates = HashMap::new();
        updates.insert("type".to_string(), JsonValue::String("test".to_string()));

        let result = execute(vault, "nonexistent", updates).await;
        assert!(result.is_err());
    }
}
