//! Remember Tool - Load session context for an explicitly named agent
//!
//! Requires an explicit `agent_id` and reads the exact conventional private
//! note `agents/<agent_id>/Working Memory.md` from the vault, plus discovered
//! project notes. It does not return the pooled `Working Memory.md`, `Log.md`,
//! or the weekly journal, and it never falls back to another note when the
//! agent's note is missing - it surfaces a visible diagnostic instead.
//! Automatically discovers projects based on git remotes and directory names.
//! Use this at the start of every session to get complete context about
//! current focus and project context for the named agent.

use std::path::Path;

use rmcp::model::{CallToolResult, Content, ErrorData, ResourceContents};

use crate::graph::GraphIndex;
use crate::projects::{DiscoveryResult, discover_projects, generate_discovery_status_message};

/// Maximum length of an agent identifier
const AGENT_ID_MAX_LEN: usize = 64;

/// Validate an explicitly supplied agent ID.
///
/// IDs are explicit lowercase ASCII identifiers: they start with a letter and
/// continue with letters, digits, hyphens, or underscores, at most
/// [`AGENT_ID_MAX_LEN`] characters. Absent, empty, and invalid IDs are
/// rejected with an invalid-params diagnostic before any context is loaded.
/// IDs are never normalized or inferred from cwd, usernames, headers, or
/// aliases.
fn validate_agent_id(agent_id: Option<&str>) -> Result<&str, ErrorData> {
    let id = agent_id.ok_or_else(|| {
        ErrorData::invalid_params(
            "remember requires an explicit agent_id: a lowercase ASCII identifier \
             (starts with a letter, then letters/digits/hyphens/underscores, at most \
             64 characters). It selects the conventional private note \
             agents/<agent_id>/Working Memory.md. IDs are never inferred from cwd, \
             usernames, or headers.",
            None,
        )
    })?;

    if is_valid_agent_id(id) {
        Ok(id)
    } else {
        Err(ErrorData::invalid_params(
            format!(
                "invalid agent_id {:?}: must be a lowercase ASCII identifier that \
                 starts with a letter, followed by letters/digits/hyphens/underscores, \
                 at most {} characters. It selects the conventional private note \
                 agents/<agent_id>/Working Memory.md; IDs are not normalized or \
                 inferred from other inputs.",
                id, AGENT_ID_MAX_LEN
            ),
            None,
        ))
    }
}

/// Check an agent ID against the conventional identifier rules: starts with a
/// lowercase ASCII letter, then lowercase ASCII letters/digits/hyphens/
/// underscores only, at most [`AGENT_ID_MAX_LEN`] characters. This also
/// rejects path traversal (`/`, `\`, `..`) since those characters are not in
/// the allowed set.
fn is_valid_agent_id(id: &str) -> bool {
    let mut chars = id.chars();
    match chars.next() {
        Some(first) if first.is_ascii_lowercase() => {}
        _ => return false,
    }
    id.chars().count() <= AGENT_ID_MAX_LEN
        && chars.all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
}

/// Execute the Remember tool for an explicit agent.
///
/// `agent_id` must be a valid conventional identifier (see
/// [`validate_agent_id`]); it selects the exact conventional private note
/// `agents/<agent_id>/Working Memory.md` relative to the vault. `cwd` remains
/// the independent project-discovery input and is used for discovery even when
/// the agent's note is missing. Missing private notes are never created, and
/// pooled context files are never substituted.
pub async fn execute(
    vault_path: &Path,
    graph_index: &GraphIndex,
    cwd: Option<&Path>,
    agent_id: Option<&str>,
) -> Result<CallToolResult, ErrorData> {
    // Reject absent/empty/invalid IDs before loading any context.
    let agent_id = validate_agent_id(agent_id)?;

    // Conventional agent-private note path: agents/<agent_id>/Working Memory.md
    let working_memory_path = vault_path
        .join("agents")
        .join(agent_id)
        .join("Working Memory.md");

    // Discover projects if CWD was provided (independent of the agent note)
    let discovery_result = cwd.map(|cwd| discover_projects(cwd, graph_index, vault_path));

    // Read the agent's private note. A missing/unreadable note is reported
    // with a visible diagnostic below - there is no fallback to another note.
    let working_memory_result = tokio::fs::read_to_string(&working_memory_path).await;
    let working_memory_loaded = working_memory_result.is_ok();

    // Visible diagnostic when the note is missing/unreadable: the exact
    // conventional path failed, nothing is created, and no other note is
    // loaded as a fallback.
    let working_memory_diagnostic = match &working_memory_result {
        Ok(_) => None,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Some(format!(
            "**No working memory note for agent '{}':** {} does not exist. \
             Nothing was created and no other note was loaded as a fallback. \
             Project notes (if any) and discovery status follow.",
            agent_id,
            working_memory_path.display()
        )),
        Err(e) => Some(format!(
            "**Could not read the working memory note for agent '{}':** {} \
             ({}). Nothing was created and no other note was loaded as a \
             fallback. Project notes (if any) and discovery status follow.",
            agent_id,
            working_memory_path.display(),
            e
        )),
    };

    // Read strict match project notes
    let mut project_contents = Vec::new();
    if let Some(ref result) = discovery_result {
        for m in &result.strict_matches {
            if let Ok(content) = tokio::fs::read_to_string(&m.metadata.file_path).await {
                project_contents.push((
                    m.metadata.file_path.clone(),
                    m.metadata.name.clone(),
                    content,
                ));
            }
        }
    }

    // Build content blocks array - one resource per file
    let mut content_blocks: Vec<Content> = Vec::new();

    if let Ok(content) = working_memory_result {
        content_blocks.push(Content::resource(ResourceContents::TextResourceContents {
            uri: format!("file://{}", working_memory_path.display()),
            mime_type: Some("text/markdown".into()),
            text: content,
            meta: None,
        }));
    }
    if let Some(diagnostic) = working_memory_diagnostic {
        content_blocks.push(Content::text(diagnostic));
    }

    // Add strictly matched project notes
    for (file_path, _name, content) in project_contents {
        content_blocks.push(Content::resource(ResourceContents::TextResourceContents {
            uri: format!("file://{}", file_path.display()),
            mime_type: Some("text/markdown".into()),
            text: content,
            meta: None,
        }));
    }

    // Generate project discovery status message
    let project_status = match (&discovery_result, cwd) {
        (Some(result), Some(cwd)) => generate_discovery_status_message(result, cwd),
        _ => "Project discovery skipped (no cwd provided).".to_string(),
    };

    // Add project status as text content
    content_blocks.push(Content::text(project_status));

    let structured = build_structured_content(
        discovery_result.as_ref(),
        agent_id,
        working_memory_loaded,
    );

    Ok(CallToolResult {
        content: content_blocks,
        is_error: None,
        meta: None,
        structured_content: Some(structured),
    })
}

/// Build structured content for the response: agent identity, whether the
/// agent's conventional working memory note loaded, and the existing project
/// discovery counts.
fn build_structured_content(
    discovery_result: Option<&DiscoveryResult>,
    agent_id: &str,
    working_memory_loaded: bool,
) -> serde_json::Value {
    let (found, disconnects, suggestions) = match discovery_result {
        Some(result) => (
            result.strict_matches.len(),
            result.loose_matches.len(),
            result.suggestions.len(),
        ),
        None => (0, 0, 0),
    };

    serde_json::json!({
        "agentId": agent_id,
        "workingMemoryLoaded": working_memory_loaded,
        "projectsFound": found,
        "projectDisconnects": disconnects,
        "projectSuggestions": suggestions,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::get_weekly_note_info;
    use std::collections::HashSet;
    use tempfile::TempDir;

    /// Marker content written to the agent-private notes
    const IRIS_MARKER: &str = "iris-private-context-marker";
    const RHEA_MARKER: &str = "rhea-private-context-marker";

    /// Poison markers planted in the old pooled context files. Agent-mode
    /// Remember must never return these.
    const POOLED_WM_POISON: &str = "POOLED-WORKING-MEMORY-POISON";
    const POOLED_LOG_POISON: &str = "POOLED-LOG-POISON";
    const POOLED_JOURNAL_POISON: &str = "POOLED-JOURNAL-POISON";

    fn create_test_vault() -> (TempDir, GraphIndex) {
        let temp_dir = TempDir::new().unwrap();
        let vault_path = temp_dir.path();

        // Old pooled context files, planted with poison markers. They and
        // their other read tools remain intact, but agent-mode Remember must
        // never return them.
        std::fs::write(vault_path.join("Working Memory.md"), POOLED_WM_POISON).unwrap();
        std::fs::write(vault_path.join("Log.md"), POOLED_LOG_POISON).unwrap();

        std::fs::create_dir_all(vault_path.join("journal")).unwrap();
        let (iso_week_date, _) = get_weekly_note_info::get_current_week_info();
        std::fs::write(
            vault_path.join(format!("journal/{}.md", iso_week_date.to_lowercase())),
            POOLED_JOURNAL_POISON,
        )
        .unwrap();

        // Conventional agent-private notes in separate conventional paths
        std::fs::create_dir_all(vault_path.join("agents/iris")).unwrap();
        std::fs::write(
            vault_path.join("agents/iris/Working Memory.md"),
            format!("### Active\n\n{}\n", IRIS_MARKER),
        )
        .unwrap();
        std::fs::create_dir_all(vault_path.join("agents/rhea")).unwrap();
        std::fs::write(
            vault_path.join("agents/rhea/Working Memory.md"),
            format!("### Active\n\n{}\n", RHEA_MARKER),
        )
        .unwrap();

        // Projects folder with a test project
        std::fs::create_dir_all(vault_path.join("projects")).unwrap();
        std::fs::write(
            vault_path.join("projects/Test Project.md"),
            "---\ntype: project\nremotes:\n  - git@github.com:user/test.git\nslug: test\n---\n\nTest project notes\n",
        )
        .unwrap();

        // Graph index with the project
        let mut graph = GraphIndex::new();
        graph.update_note(
            "Test Project",
            std::path::PathBuf::from("projects/Test Project.md"),
            HashSet::new(),
        );

        (temp_dir, graph)
    }

    /// Create a test cwd whose git remote matches the fixture project
    fn create_matching_cwd(parent: &Path) -> std::path::PathBuf {
        let test_cwd = parent.join("test-project");
        std::fs::create_dir_all(&test_cwd).unwrap();

        // Initialize git repo with matching remote
        std::process::Command::new("git")
            .args(["init"])
            .current_dir(&test_cwd)
            .output()
            .ok();
        std::process::Command::new("git")
            .args(["remote", "add", "origin", "git@github.com:user/test.git"])
            .current_dir(&test_cwd)
            .output()
            .ok();

        test_cwd
    }

    fn resource_texts(result: &CallToolResult) -> Vec<String> {
        result
            .content
            .iter()
            .filter_map(|c| match &c.raw {
                rmcp::model::RawContent::Resource(embedded) => match &embedded.resource {
                    ResourceContents::TextResourceContents { text, .. } => Some(text.clone()),
                    _ => None,
                },
                _ => None,
            })
            .collect()
    }

    fn text_blocks(result: &CallToolResult) -> Vec<String> {
        result
            .content
            .iter()
            .filter_map(|c| c.raw.as_text().map(|t| t.text.clone()))
            .collect()
    }

    #[tokio::test]
    async fn test_remember_loads_agent_note_and_skips_pooled_files() {
        let (temp_dir, graph) = create_test_vault();
        let vault_path = temp_dir.path();

        let result = execute(vault_path, &graph, None, Some("iris"))
            .await
            .unwrap();

        // The agent's private note is returned, with its distinctive marker
        let resources = resource_texts(&result);
        assert!(
            resources.iter().any(|r| r.contains(IRIS_MARKER)),
            "expected the iris agent note, got: {:?}",
            resources
        );

        // Pooled files are never returned
        let all_text = format!("{:?}\n{:?}", resources, text_blocks(&result));
        assert!(!all_text.contains(POOLED_WM_POISON), "pooled Working Memory.md leaked");
        assert!(!all_text.contains(POOLED_LOG_POISON), "pooled Log.md leaked");
        assert!(!all_text.contains(POOLED_JOURNAL_POISON), "weekly journal leaked");

        // Structured content identifies the agent and the loaded note
        let structured = result.structured_content.clone().unwrap();
        assert_eq!(structured["agentId"], "iris");
        assert_eq!(structured["workingMemoryLoaded"], true);
        assert_eq!(structured["projectsFound"], 0);
        assert_eq!(structured["projectDisconnects"], 0);
        assert_eq!(structured["projectSuggestions"], 0);

        // No fallback diagnostic when the note exists
        assert!(!all_text.contains("No working memory note"));
    }

    #[tokio::test]
    async fn test_two_agent_ids_in_one_cwd() {
        let (temp_dir, graph) = create_test_vault();
        let vault_path = temp_dir.path();
        let cwd = create_matching_cwd(temp_dir.path());

        let iris = execute(vault_path, &graph, Some(&cwd), Some("iris"))
            .await
            .unwrap();
        let rhea = execute(vault_path, &graph, Some(&cwd), Some("rhea"))
            .await
            .unwrap();

        // Each agent gets its own private note, not the other's
        let iris_resources = resource_texts(&iris);
        let rhea_resources = resource_texts(&rhea);
        assert!(iris_resources.iter().any(|r| r.contains(IRIS_MARKER)));
        assert!(!iris_resources.iter().any(|r| r.contains(RHEA_MARKER)));
        assert!(rhea_resources.iter().any(|r| r.contains(RHEA_MARKER)));
        assert!(!rhea_resources.iter().any(|r| r.contains(IRIS_MARKER)));

        // Project discovery is retained for both agents in the same cwd
        for result in [&iris, &rhea] {
            let structured = result.structured_content.clone().unwrap();
            assert_eq!(structured["projectsFound"], 1);
            assert_eq!(structured["workingMemoryLoaded"], true);
        }
        let iris_structured = iris.structured_content.clone().unwrap();
        assert_eq!(iris_structured["agentId"], "iris");
        let rhea_structured = rhea.structured_content.clone().unwrap();
        assert_eq!(rhea_structured["agentId"], "rhea");
    }

    #[tokio::test]
    async fn test_remember_rejects_absent_agent_id() {
        let (temp_dir, graph) = create_test_vault();

        let err = execute(temp_dir.path(), &graph, None, None)
            .await
            .expect_err("absent agent_id must be rejected before loading context");
        assert!(err.message.contains("agent_id"), "got: {}", err.message);
    }

    #[tokio::test]
    async fn test_remember_rejects_invalid_agent_id_with_diagnostic() {
        let (temp_dir, graph) = create_test_vault();

        let invalid_ids: Vec<String> = vec![
            "".into(),                // empty
            "Iris".into(),            // uppercase
            "1iris".into(),           // must start with a letter
            "_iris".into(),           // must start with a letter
            "iris agent".into(),      // space
            "iris!".into(),           // punctuation
            "iris/../../Log".into(),  // path traversal
            "../escape".into(),       // path traversal
            "iris\n".into(),          // newline
            "a".repeat(65),           // 65 chars, one over the limit
        ];

        for id in invalid_ids {
            let err = match execute(temp_dir.path(), &graph, None, Some(&id)).await {
                Err(err) => err,
                Ok(_) => panic!("execute must reject invalid agent_id {:?}", id),
            };
            assert!(
                err.message.contains("agent_id"),
                "diagnostic should name the agent_id contract, got: {}",
                err.message
            );
        }
    }

    #[tokio::test]
    async fn test_remember_rejects_overlong_agent_id() {
        let (temp_dir, graph) = create_test_vault();

        let ok_len = "a".repeat(64);
        execute(temp_dir.path(), &graph, None, Some(&ok_len))
            .await
            .expect("64-char ID is allowed");

        let too_long = "a".repeat(65);
        let err = execute(temp_dir.path(), &graph, None, Some(&too_long))
            .await
            .expect_err("65-char ID must be rejected");
        assert!(err.message.contains("agent_id"), "got: {}", err.message);
    }

    #[tokio::test]
    async fn test_remember_valid_id_missing_note_diagnostic_no_fallback() {
        let temp_dir = TempDir::new().unwrap();
        let vault_path = temp_dir.path();
        let graph = GraphIndex::new();

        // No agents/tada directory exists at all
        let result = execute(vault_path, &graph, None, Some("tada"))
            .await
            .unwrap();

        // Visible diagnostic naming the agent and the exact conventional path
        let texts = text_blocks(&result).join("\n");
        assert!(
            texts.contains("No working memory note") && texts.contains("tada"),
            "expected a visible missing-note diagnostic, got: {}",
            texts
        );

        // No fallback: none of the pooled files (not even present here) or
        // other notes were loaded - only text blocks, no resources
        let resources = resource_texts(&result);
        assert!(
            resources.is_empty(),
            "no note resource should be loaded when the agent note is missing, got: {:?}",
            resources
        );

        // Structured output reports the miss
        let structured = result.structured_content.clone().unwrap();
        assert_eq!(structured["agentId"], "tada");
        assert_eq!(structured["workingMemoryLoaded"], false);
    }

    #[tokio::test]
    async fn test_remember_missing_note_still_discovers_projects() {
        let (temp_dir, graph) = create_test_vault();
        let vault_path = temp_dir.path();
        let cwd = create_matching_cwd(temp_dir.path());

        // Valid ID with no note on disk
        let result = execute(vault_path, &graph, Some(&cwd), Some("t271"))
            .await
            .unwrap();

        let structured = result.structured_content.clone().unwrap();
        assert_eq!(structured["agentId"], "t271");
        assert_eq!(structured["workingMemoryLoaded"], false);
        // Useful project discovery is preserved despite the missing note
        assert_eq!(structured["projectsFound"], 1);

        let resources = resource_texts(&result);
        assert!(
            resources.iter().any(|r| r.contains("Test project notes")),
            "discovered project note should still be loaded, got: {:?}",
            resources
        );
    }

    #[tokio::test]
    async fn test_remember_rereads_agent_note_after_edit() {
        let (temp_dir, graph) = create_test_vault();
        let vault_path = temp_dir.path();

        let first = execute(vault_path, &graph, None, Some("iris"))
            .await
            .unwrap();
        assert!(resource_texts(&first).iter().any(|r| r.contains(IRIS_MARKER)));

        // Edit the note on disk (as an editor would), then reread
        std::fs::write(
            vault_path.join("agents/iris/Working Memory.md"),
            "### Active\n\nupdated-after-edit-marker\n",
        )
        .unwrap();

        let second = execute(vault_path, &graph, None, Some("iris"))
            .await
            .unwrap();
        let resources = resource_texts(&second);
        assert!(
            resources.iter().any(|r| r.contains("updated-after-edit-marker")),
            "second read must see the edited note, got: {:?}",
            resources
        );
        assert!(
            !resources.iter().any(|r| r.contains(IRIS_MARKER)),
            "stale content must not be returned after the edit"
        );
        assert_eq!(second.structured_content.clone().unwrap()["workingMemoryLoaded"], true);
    }

    #[tokio::test]
    async fn test_remember_agent_note_read_is_exact_not_lookup() {
        let (temp_dir, graph) = create_test_vault();
        let vault_path = temp_dir.path();

        // A decoy note whose basename matches semantically must NOT be
        // selected - only the exact conventional path counts.
        std::fs::write(vault_path.join("iris.md"), "semantic-lookup-decoy").unwrap();

        let result = execute(vault_path, &graph, None, Some("iris"))
            .await
            .unwrap();
        let resources = resource_texts(&result);
        assert!(resources.iter().any(|r| r.contains(IRIS_MARKER)));
        assert!(
            !resources.iter().any(|r| r.contains("semantic-lookup-decoy")),
            "basename/semantic lookup must not be used, got: {:?}",
            resources
        );
    }

    #[tokio::test]
    async fn test_remember_without_cwd_skips_discovery() {
        let (temp_dir, graph) = create_test_vault();
        let vault_path = temp_dir.path();

        let result = execute(vault_path, &graph, None, Some("iris"))
            .await
            .unwrap();

        let structured = result.structured_content.clone().unwrap();
        assert_eq!(structured["projectsFound"], 0);
        assert_eq!(structured["workingMemoryLoaded"], true);

        let texts = text_blocks(&result).join("\n");
        assert!(texts.contains("skipped"));
    }

    #[tokio::test]
    async fn test_valid_agent_id_rules() {
        assert!(is_valid_agent_id("a"));
        assert!(is_valid_agent_id("iris"));
        assert!(is_valid_agent_id("rhea-2"));
        assert!(is_valid_agent_id("agent_01-x"));
        assert!(is_valid_agent_id(&"a".repeat(64)));

        assert!(!is_valid_agent_id(""));
        assert!(!is_valid_agent_id("Iris"));
        assert!(!is_valid_agent_id("1agent"));
        assert!(!is_valid_agent_id("-agent"));
        assert!(!is_valid_agent_id("_agent"));
        assert!(!is_valid_agent_id("ag ent"));
        assert!(!is_valid_agent_id("agént"));
        assert!(!is_valid_agent_id("agent/id"));
        assert!(!is_valid_agent_id("../escape"));
        assert!(!is_valid_agent_id(&"a".repeat(65)));
    }
}
