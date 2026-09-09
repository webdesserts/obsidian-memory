//! Reflect tool - returns consolidation instructions for memory cleanup.
//!
//! The Reflect tool reviews explicitly available work and activity context
//! (the caller's working memory or other auto-loaded context notes, the
//! current weekly journal, project notes) and returns detailed instructions
//! for consolidating content into permanent storage. It doesn't perform the
//! consolidation itself - it provides a comprehensive prompt that guides the
//! agent through the process.

use rmcp::model::{CallToolResult, Content, ErrorData};

/// Execute the Reflect tool - returns consolidation instructions.
pub fn execute() -> Result<CallToolResult, ErrorData> {
    let prompt = build_reflect_prompt();
    Ok(CallToolResult::success(vec![Content::text(prompt)]))
}

/// Build the comprehensive consolidation prompt.
fn build_reflect_prompt() -> String {
    r#"# Memory Consolidation

You are performing a focused consolidation session to optimize token usage while preserving important memories. Review active context and consolidate content into permanent storage.

## Information Lifecycle

**Active work (keep details):**
- Work currently in progress
- Decisions still being evaluated
- Context needed for ongoing tasks

**Shipped/merged work (compress):**
- Completed features → brief summary with key decisions
- Resolved bugs → one-line description of cause and fix
- Merged PRs → link + outcome

**Outdated/irrelevant (remove):**
- Superseded approaches
- Abandoned ideas
- Temporary debugging context

## Consolidation Techniques

Edit only notes covered by the user's grant. Preserve authoritative source records, historical archives and external evidence: summarize and link to them rather than deleting them after consolidation.

1. **Forget** - Remove incorrect, irrelevant, or obvious information
   - Search first to avoid leaving phantom memories in other notes

2. **Compact** - Rewrite concisely while preserving essential information
   - Example: Detailed debugging steps → "Fixed X by doing Y"

3. **Migrate** - **Move** information from mutable notes you are authorized to consolidate into appropriate permanent notes. Remove the migrated note content only after verifying the destination; avoid keeping duplicate active copies.
   - Working Memory sections → knowledge notes or project notes
   - Mutable activity notes → weekly journal narratives or knowledge notes
   - Optional: leave a one-line wiki-link breadcrumb at the source pointing to the new home

4. **Fragment** - Split large notes into smaller focused notes
   - Use wiki-links to connect fragments

## Consolidating activity into weekly narratives

When distilling work and activity records into the weekly journal, create episodic narratives:
- Write from collaborative "we" perspective
- Include the thought process and discoveries
- Use bold summaries for browsability
- Group related work into coherent stories
- After verifying a narrative, remove migrated content only from mutable notes covered by the consolidation grant; preserve and link authoritative source records

## Your Task

1. **Review** - Read through the explicitly available work and activity sources, such as:
   - Your working memory note (e.g. the context loaded by Remember)
   - Current weekly journal
   - Discovered/loaded project notes
   - Any other work or activity records explicitly available to you (recent reports, board summaries, maintenance notes)

2. **Categorize** - For each piece of content, decide:
   - Keep in labile notes (still active)
   - Compact (summarize)
   - Migrate to permanent note (specify which)
   - Remove (no longer relevant)

3. **Propose** - Show the user what changes you want to make:
   - Deletions from Working Memory
   - Compressions in working memory notes
   - New content for weekly journal
   - Updates to knowledge/project notes

4. **Apply** - After user approval:
   - Use Write tool for note updates
   - Verify changes with Read tool

5. **Report** - Summarize what was consolidated:
   - Tokens saved (approximate)
   - Notes updated
   - Information preserved vs. removed

## Token Targets

- Auto-loaded files (Remember): <10k tokens combined
- Individual notes: ~2.5k token soft cap
- If a note exceeds limits, fragment into focused sub-notes

Begin by reading the active context files, then propose your consolidation plan.
"#
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_execute_returns_success() {
        let result = execute();
        assert!(result.is_ok());

        let call_result = result.unwrap();
        assert!(!call_result.is_error.unwrap_or(false));
    }

    #[test]
    fn test_prompt_contains_key_sections() {
        let result = execute().unwrap();
        let content = result.content[0]
            .raw
            .as_text()
            .expect("Expected text content");

        assert!(content.text.contains("Information Lifecycle"));
        assert!(content.text.contains("Consolidation Techniques"));
        assert!(content.text.contains("Your Task"));
        assert!(content.text.contains("Token Targets"));
    }

    // Owning-boundary check (t272): Reflect is instruction-only, so its static
    // prompt is the API. It must no longer direct active Log maintenance —
    // the Log/WriteLogs MCP tools are retired and no advertised workflow may
    // send agents back to append-to-Log or rewrite-Log routes.
    #[test]
    fn test_prompt_does_not_direct_active_log_maintenance() {
        let result = execute().unwrap();
        let content = result.content[0]
            .raw
            .as_text()
            .expect("Expected text content");

        for forbidden in ["Log.md", "WriteLogs", "Log tool", "log entries"] {
            assert!(
                !content.text.contains(forbidden),
                "Reflect instructions must not reference {forbidden} (no active Log maintenance)"
            );
        }
    }

    // Reflect's guidance must stay generic to explicitly available
    // work/activity sources - it must not hardcode a particular agent's URLs
    // or grow its own identity/config registry.
    #[test]
    fn test_prompt_targets_generic_explicit_sources() {
        let result = execute().unwrap();
        let content = result.content[0]
            .raw
            .as_text()
            .expect("Expected text content");

        for expected in [
            "working memory",
            "weekly journal",
            "project notes",
            "work or activity records explicitly available",
        ] {
            assert!(
                content.text.contains(expected),
                "Reflect instructions should name the generic source {expected}"
            );
        }

        for forbidden in ["Umbra", "Autonomy", "http://", "https://", "agent_id"] {
            assert!(
                !content.text.contains(forbidden),
                "Reflect instructions must not hardcode {forbidden}"
            );
        }
    }
}
