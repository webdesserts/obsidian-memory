use obsidian_fs::{
    ensure_markdown_extension, generate_search_paths, normalize_note_reference,
    parse_frontmatter, NoteRef,
};
use rmcp::model::{CallToolResult, Content, ErrorData};
use std::path::Path;
use tokio::fs;

use crate::graph::GraphIndex;

/// Result structure for GetNote tool
pub struct NoteInfo {
    pub note_name: String,
    pub path: String,
    pub file_path: String,
    pub memory_uri: String,
    pub obsidian_uri: String,
    pub exists: bool,
    pub forward_links: Vec<String>,
    pub backlinks: Vec<String>,
    pub frontmatter_keys: Vec<String>,
}

/// Resolve a note reference to a file path, searching the vault if needed.
///
/// Returns (resolved_path_without_ext, exists)
async fn resolve_note_to_path(
    vault_path: &Path,
    note_ref: &NoteRef,
    graph: &GraphIndex,
) -> (String, bool) {
    // First, check if the note is in the graph index
    if let Some(graph_path) = graph.get_path(&note_ref.name) {
        // Found in graph - convert PathBuf to string without .md extension
        let path_str = graph_path.to_string_lossy();
        let path_without_ext = path_str.strip_suffix(".md").unwrap_or(&path_str);
        return (path_without_ext.to_string(), true);
    }

    // Not in graph - try to find file on disk
    // If the reference includes a path (e.g., "knowledge/Note"), try that first
    if note_ref.path.contains('/') {
        let file_path = vault_path.join(ensure_markdown_extension(&note_ref.path));
        if file_path.exists() {
            return (note_ref.path.clone(), true);
        }
    }

    // Generate search paths and try each one
    let search_paths = generate_search_paths(&note_ref.name, false);
    for search_path in &search_paths {
        let file_path = vault_path.join(ensure_markdown_extension(search_path));
        if file_path.exists() {
            return (search_path.clone(), true);
        }
    }

    // Not found anywhere - return the original path
    (note_ref.path.clone(), false)
}

/// Execute the GetNote tool
pub async fn execute(
    vault_path: &Path,
    vault_name: &str,
    graph: &GraphIndex,
    note: &str,
) -> Result<CallToolResult, ErrorData> {
    // Normalize the note reference
    let note_ref = normalize_note_reference(note);
    let note_name = note_ref.name.clone();

    // Resolve to actual path
    let (resolved_path, exists) = resolve_note_to_path(vault_path, &note_ref, graph).await;

    // Build URIs
    let file_path = vault_path
        .join(ensure_markdown_extension(&resolved_path))
        .to_string_lossy()
        .to_string();
    let memory_uri = format!("memory:{}", resolved_path);
    let obsidian_uri = format!(
        "obsidian://open?vault={}&file={}",
        urlencoding::encode(vault_name),
        urlencoding::encode(&resolved_path)
    );

    if !exists {
        // Note doesn't exist - return helpful message
        let text = format!(
            "Note not found: {}\n\n\
             This note doesn't exist yet. You can create it at:\n\
             - File path: {}\n\
             - Memory URI: {}\n\
             - Obsidian URI: {}\n\n\
             Use the Write tool with the file path to create this note.",
            note_name, file_path, memory_uri, obsidian_uri
        );
        return Ok(CallToolResult::success(vec![Content::text(text)]));
    }

    // Note exists - get links and frontmatter
    let forward_links: Vec<String> = graph
        .get_forward_links(&note_name)
        .map(|links| {
            links
                .iter()
                .map(|link| {
                    let path = graph
                        .get_path(link)
                        .map(|p| p.to_string_lossy().replace(".md", ""))
                        .unwrap_or_else(|| link.clone());
                    format!("memory:{}", path)
                })
                .collect()
        })
        .unwrap_or_default();

    let backlinks: Vec<String> = graph
        .get_backlinks(&note_name)
        .map(|links| {
            links
                .iter()
                .map(|link| {
                    let path = graph
                        .get_path(link)
                        .map(|p| p.to_string_lossy().replace(".md", ""))
                        .unwrap_or_else(|| link.clone());
                    format!("memory:{}", path)
                })
                .collect()
        })
        .unwrap_or_default();

    // Read frontmatter
    let frontmatter_keys: Vec<String> = match fs::read_to_string(&file_path).await {
        Ok(content) => {
            let parsed = parse_frontmatter(&content);
            parsed
                .frontmatter
                .map(|fm| fm.keys().cloned().collect())
                .unwrap_or_default()
        }
        Err(_) => Vec::new(),
    };

    // Build response text
    let links_summary = if !forward_links.is_empty() {
        format!("\n\nLinks to: {}", forward_links.join(", "))
    } else {
        String::new()
    };

    let backlinks_summary = if !backlinks.is_empty() {
        format!("\n\nLinked from: {}", backlinks.join(", "))
    } else {
        String::new()
    };

    let frontmatter_summary = if !frontmatter_keys.is_empty() {
        format!("\n\nFrontmatter: {}", frontmatter_keys.join(", "))
    } else {
        String::new()
    };

    let text = format!(
        "Note: {}\n\
         Path: {}\n\
         File: {}\n\
         Memory URI: {}{}{}{}\n\n\
         Use Read tool with the file path to view content.\n\
         Use Write tool with the file path to edit content.",
        note_name,
        resolved_path,
        file_path,
        memory_uri,
        links_summary,
        backlinks_summary,
        frontmatter_summary
    );

    Ok(CallToolResult::success(vec![Content::text(text)]))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use tempfile::TempDir;

    async fn create_test_vault() -> (TempDir, GraphIndex) {
        let temp_dir = TempDir::new().unwrap();
        let vault_path = temp_dir.path();

        // Create some test notes
        fs::create_dir_all(vault_path.join("knowledge")).await.unwrap();

        // Note A links to Note B
        fs::write(
            vault_path.join("knowledge/Note A.md"),
            "---\ntype: test\n---\n\nContent linking to [[Note B]]",
        )
        .await
        .unwrap();

        // Note B
        fs::write(
            vault_path.join("knowledge/Note B.md"),
            "---\ntags: [one, two]\n---\n\nNote B content",
        )
        .await
        .unwrap();

        // Build graph
        let mut graph = GraphIndex::new();
        graph.update_note(
            "Note A",
            "knowledge/Note A.md".into(),
            ["Note B".to_string()].into_iter().collect(),
        );
        graph.update_note(
            "Note B",
            "knowledge/Note B.md".into(),
            HashSet::new(),
        );

        (temp_dir, graph)
    }

    #[tokio::test]
    async fn test_get_existing_note() {
        let (temp_dir, graph) = create_test_vault().await;

        let result = execute(
            temp_dir.path(),
            "test-vault",
            &graph,
            "Note A",
        )
        .await
        .expect("should succeed");

        let text = result.content[0]
            .raw
            .as_text()
            .expect("Expected text")
            .text
            .clone();

        assert!(text.contains("Note: Note A"));
        assert!(text.contains("knowledge/Note A"));
        assert!(text.contains("memory:knowledge/Note A"));
    }

    #[tokio::test]
    async fn test_get_nonexistent_note() {
        let (temp_dir, graph) = create_test_vault().await;

        let result = execute(
            temp_dir.path(),
            "test-vault",
            &graph,
            "Nonexistent Note",
        )
        .await
        .expect("should succeed");

        let text = result.content[0]
            .raw
            .as_text()
            .expect("Expected text")
            .text
            .clone();

        assert!(text.contains("Note not found"));
        assert!(text.contains("Nonexistent Note"));
    }

    #[tokio::test]
    async fn test_get_note_with_links() {
        let (temp_dir, graph) = create_test_vault().await;

        let result = execute(
            temp_dir.path(),
            "test-vault",
            &graph,
            "Note A",
        )
        .await
        .expect("should succeed");

        let text = result.content[0]
            .raw
            .as_text()
            .expect("Expected text")
            .text
            .clone();

        // Should show forward links
        assert!(text.contains("Links to:"));
        assert!(text.contains("Note B"));
    }

    #[tokio::test]
    async fn test_get_note_with_backlinks() {
        let (temp_dir, graph) = create_test_vault().await;

        let result = execute(
            temp_dir.path(),
            "test-vault",
            &graph,
            "Note B",
        )
        .await
        .expect("should succeed");

        let text = result.content[0]
            .raw
            .as_text()
            .expect("Expected text")
            .text
            .clone();

        // Should show backlinks
        assert!(text.contains("Linked from:"));
        assert!(text.contains("Note A"));
    }

    #[tokio::test]
    async fn test_normalizes_note_reference() {
        let (temp_dir, graph) = create_test_vault().await;

        // Test with memory: URI
        let result = execute(
            temp_dir.path(),
            "test-vault",
            &graph,
            "memory:knowledge/Note A",
        )
        .await
        .expect("should succeed");

        let text = result.content[0]
            .raw
            .as_text()
            .expect("Expected text")
            .text
            .clone();

        assert!(text.contains("Note: Note A"));
    }
}
