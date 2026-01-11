//! LoadPrivateMemory tool - loads private memory indexes with explicit consent.
//!
//! Private notes contain sensitive information (work-related, personal) that
//! shouldn't be automatically loaded. This tool requires the agent to explain
//! why it needs access, creating a consent-based access model.

use rmcp::model::{CallToolResult, Content, ErrorData};
use std::path::Path;
use tokio::fs;

/// Execute the LoadPrivateMemory tool.
///
/// Loads the private Working Memory file and returns its content along with
/// a list of available private knowledge notes.
pub async fn execute(vault_path: &Path, reason: &str) -> Result<CallToolResult, ErrorData> {
    if reason.trim().is_empty() {
        return Err(ErrorData::invalid_params(
            "A reason for loading private memory is required".to_string(),
            None,
        ));
    }

    let private_dir = vault_path.join("private");
    let private_wm_path = private_dir.join("Working Memory.md");

    // Check if private directory exists
    if !private_dir.exists() {
        return Ok(CallToolResult::success(vec![Content::text(
            "No private memory directory found. Create `private/` folder in your vault to use private memory."
        )]));
    }

    let mut result_parts = Vec::new();

    // Load private Working Memory if it exists
    match fs::read_to_string(&private_wm_path).await {
        Ok(content) => {
            result_parts.push(format!(
                "## private/Working Memory.md\n\n{}",
                content
            ));
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            result_parts.push(
                "## private/Working Memory.md\n\nFile does not exist. Create it to store private working memory."
                    .to_string(),
            );
        }
        Err(e) => {
            return Err(ErrorData::internal_error(
                format!("Failed to read private Working Memory: {}", e),
                None,
            ));
        }
    }

    // List available private knowledge notes
    let mut private_notes = Vec::new();
    if let Ok(mut entries) = fs::read_dir(&private_dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let path = entry.path();
            if path.extension().map(|e| e == "md").unwrap_or(false) {
                if let Some(name) = path.file_stem() {
                    let name_str = name.to_string_lossy();
                    // Skip Working Memory since we already loaded it
                    if name_str != "Working Memory" {
                        private_notes.push(name_str.to_string());
                    }
                }
            }
        }
    }

    if !private_notes.is_empty() {
        private_notes.sort();
        result_parts.push(format!(
            "\n## Available Private Notes\n\n{}",
            private_notes
                .iter()
                .map(|n| format!("- [[private/{}]]", n))
                .collect::<Vec<_>>()
                .join("\n")
        ));
    }

    result_parts.push(format!(
        "\n---\n\n*Private memory loaded. Reason: {}*",
        reason
    ));

    Ok(CallToolResult::success(vec![Content::text(
        result_parts.join("\n"),
    )]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_requires_reason() {
        let temp_dir = TempDir::new().unwrap();
        let result = execute(temp_dir.path(), "").await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn test_no_private_dir() {
        let temp_dir = TempDir::new().unwrap();
        let result = execute(temp_dir.path(), "Testing").await;
        assert!(result.is_ok());

        let call_result = result.unwrap();
        let content = call_result.content[0]
            .raw
            .as_text()
            .expect("Expected text content");
        assert!(content.text.contains("No private memory directory"));
    }

    #[tokio::test]
    async fn test_loads_private_working_memory() {
        let temp_dir = TempDir::new().unwrap();
        let private_dir = temp_dir.path().join("private");
        fs::create_dir(&private_dir).await.unwrap();

        let wm_content = "# Private Working Memory\n\nSensitive notes here.";
        fs::write(private_dir.join("Working Memory.md"), wm_content)
            .await
            .unwrap();

        let result = execute(temp_dir.path(), "Need to check work notes").await;
        assert!(result.is_ok());

        let call_result = result.unwrap();
        let content = call_result.content[0]
            .raw
            .as_text()
            .expect("Expected text content");
        assert!(content.text.contains("Sensitive notes here"));
        assert!(content.text.contains("Need to check work notes"));
    }

    #[tokio::test]
    async fn test_lists_private_notes() {
        let temp_dir = TempDir::new().unwrap();
        let private_dir = temp_dir.path().join("private");
        fs::create_dir(&private_dir).await.unwrap();

        fs::write(private_dir.join("Working Memory.md"), "# WM")
            .await
            .unwrap();
        fs::write(private_dir.join("Work Project.md"), "# Work")
            .await
            .unwrap();
        fs::write(private_dir.join("Personal.md"), "# Personal")
            .await
            .unwrap();

        let result = execute(temp_dir.path(), "Checking notes").await;
        assert!(result.is_ok());

        let call_result = result.unwrap();
        let content = call_result.content[0]
            .raw
            .as_text()
            .expect("Expected text content");
        assert!(content.text.contains("[[private/Work Project]]"));
        assert!(content.text.contains("[[private/Personal]]"));
        // Should not list Working Memory in the available notes (it's shown separately)
        assert!(!content.text.contains("[[private/Working Memory]]"));
    }
}
