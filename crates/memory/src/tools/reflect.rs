//! Reflect tool - returns consolidation instructions for memory cleanup.
//!
//! The Reflect tool reviews active context (Log.md, Working Memory.md, current weekly
//! journal, project notes) and returns detailed instructions for consolidating content
//! into permanent storage. It doesn't perform the consolidation itself - it provides
//! a comprehensive prompt that guides the agent through the process.

use rmcp::model::{CallToolResult, Content, ErrorData};

/// Execute the Reflect tool - returns consolidation instructions.
pub fn execute(include_private: bool) -> Result<CallToolResult, ErrorData> {
    let prompt = build_reflect_prompt(include_private);
    Ok(CallToolResult::success(vec![Content::text(prompt)]))
}

/// Build the comprehensive consolidation prompt.
fn build_reflect_prompt(include_private: bool) -> String {
    let private_section = if include_private {
        r#"
## Private Memory

You have access to private memory for this session. Include `private/Working Memory.md` 
in your review alongside the regular Working Memory. Private content should consolidate 
to `private/*.md` knowledge notes, not public ones.
"#
    } else {
        ""
    };

    format!(
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

1. **Forget** - Remove incorrect, irrelevant, or obvious information
   - Search first to avoid leaving phantom memories in other notes
   
2. **Compact** - Rewrite concisely while preserving essential information
   - Example: Detailed debugging steps → "Fixed X by doing Y"
   
3. **Migrate** - Move information to appropriate permanent notes
   - Working Memory sections → knowledge notes or project notes
   - Log entries → weekly journal summaries
   
4. **Fragment** - Split large notes into smaller focused notes
   - Use wiki-links to connect fragments

## Log.md Format

Log uses ISO week dates with day abbreviations:

```
## 2025-W48-1 (Mon)

- 10:29 AM – Started investigation into bug
- 2:15 PM – Found root cause in auth module
```

When consolidating logs to weekly journal, create episodic narratives:
- Write from collaborative "we" perspective
- Include the thought process and discoveries
- Use bold summaries for browsability
- Group related work into coherent stories
{private_section}
## Your Task

1. **Review** - Read through:
   - Log.md (recent entries)
   - Working Memory.md
   - Current weekly journal
   - Any loaded project notes

2. **Categorize** - For each piece of content, decide:
   - Keep in labile notes (still active)
   - Compact (summarize)
   - Migrate to permanent note (specify which)
   - Remove (no longer relevant)

3. **Propose** - Show the user what changes you want to make:
   - Deletions from Working Memory
   - Compressions in Log
   - New content for weekly journal
   - Updates to knowledge/project notes

4. **Apply** - After user approval:
   - Use Write tool for note updates
   - Use WriteLogs tool for Log.md changes
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
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_execute_returns_success() {
        let result = execute(false);
        assert!(result.is_ok());

        let call_result = result.unwrap();
        assert!(!call_result.is_error.unwrap_or(false));
    }

    #[test]
    fn test_prompt_contains_key_sections() {
        let result = execute(false).unwrap();
        let content = result.content[0]
            .raw
            .as_text()
            .expect("Expected text content");

        assert!(content.text.contains("Information Lifecycle"));
        assert!(content.text.contains("Consolidation Techniques"));
        assert!(content.text.contains("Log.md Format"));
        assert!(content.text.contains("Your Task"));
        assert!(content.text.contains("Token Targets"));
    }

    #[test]
    fn test_private_flag_includes_private_section() {
        let result = execute(true).unwrap();
        let content = result.content[0]
            .raw
            .as_text()
            .expect("Expected text content");

        assert!(content.text.contains("Private Memory"));
        assert!(content.text.contains("private/Working Memory.md"));
    }

    #[test]
    fn test_no_private_flag_excludes_private_section() {
        let result = execute(false).unwrap();
        let content = result.content[0]
            .raw
            .as_text()
            .expect("Expected text content");

        assert!(!content.text.contains("Private Memory"));
    }
}
