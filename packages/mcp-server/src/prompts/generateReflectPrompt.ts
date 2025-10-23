/**
 * Generate reflect prompt for memory consolidation
 *
 * This is a pure function that generates the prompt content for the reflect workflow.
 * It can be used by both MCP prompts and tools.
 */

export interface ReflectPromptOptions {
  workingMemoryContent: string;
  weeklyNotePath: string;
  currentWeekNumber: number;
  currentDayOfWeek: string;
  includePrivate: boolean;
}

export interface PromptMessage {
  role: "user" | "assistant";
  content: {
    type: "text";
    text: string;
  };
}

export async function generateReflectPrompt(
  options: ReflectPromptOptions
): Promise<{ messages: PromptMessage[] }> {
  const { workingMemoryContent, weeklyNotePath, currentWeekNumber, currentDayOfWeek, includePrivate } = options;

  const promptText = `# Memory Reflection

Review Working Memory and consolidate into permanent notes.

## Working Memory

${workingMemoryContent || "*(Working Memory is empty)*"}

## Current Week's Journal

Path: ${weeklyNotePath}
**Today is ${currentDayOfWeek}, week ${currentWeekNumber}**

## Consolidation Workflow

### Phase 1: Categorize

Review the **Timeline** and **Notes** sections in Working Memory. Categorize each piece of content by its destination:

1. **Knowledge notes** - Technical facts, APIs, patterns, how things work
   - Term-based, small, focused (dictionary-style)
   - Example: \`knowledge/React Server Components.md\`, \`knowledge/MCP Prompts.md\`
   - Keep these concise - think encyclopedia entries, not articles

2. **Project notes** - Design decisions, architecture, project context
   - Deep dives on specific projects
   - Example: \`knowledge/Obsidian Memory Project.md\`
   - Can be longer and more detailed than knowledge notes

3. **Weekly journal Log** - Work summaries
   - Add under **"## Log"** header in current week's journal
   - **Review timeline entries and backfill logs for the appropriate days**
   - If Working Memory has entries from previous days, add them under their respective weekday sub-headers
   - Today is **${currentDayOfWeek}**, so current entries go under \`### ${currentDayOfWeek}\`
   - Previous days' entries go under \`### Monday\`, \`### Tuesday\`, etc.
   - Consolidate timeline entries into readable summary (not verbatim copy)
   - Link to relevant [[Project]] and [[Knowledge]] notes
   - Keep entries concise with bullet points

4. **Discard** - Not valuable long-term
   - Routine fixes, temporary notes, already-documented info

### Phase 2: Propose Changes

For each piece of content you're keeping, show a clear proposal with enough context for review.

**Format:**

\`\`\`
## Weekly Journal: ${weeklyNotePath}
**Action:** Update existing

**Section:** Log → Monday (backfill from previous session)
**Add:**
- Started work on [[Obsidian Memory Project]]
  - Researched MCP prompts vs commands

**Section:** Log → ${currentDayOfWeek} (today's entries)
**Add:**
- Worked on [[Obsidian Memory Project]]
  - Renamed consolidation to reindex
  - Implemented reflect prompt for memory cleanup
- Reviewed [[MCP Servers]] documentation
  - Added section on prompts vs tools

## Knowledge Note: MCP Prompts
**Action:** Create new
**Path:** knowledge/MCP Prompts.md
**Content preview:**
> Reusable prompt templates that MCP servers expose to clients...
> (show enough content for user to review)

## Project Note: Obsidian Memory Project
**Action:** Update existing
**Section:** Implementation Status
**Add:**
- Implemented reflect prompt workflow
- Separated reindex and reflect concerns
\`\`\`

Show clear, reviewable proposals. Include enough content that the user can see what's being added.

### Phase 3: Get Approval

After showing all proposed changes, ask:

**"Review the proposed changes above. Should I proceed with applying them? You can edit any proposals before approving."**

Wait for explicit approval. Don't proceed without it.

### Phase 4: Apply Changes (after approval)

Once approved, apply the changes:

1. Use \`get_note()\` to check if notes exist
2. Use \`Read()\` to load existing note content
3. Use \`Write()\` to save updated notes
4. Use \`get_weekly_note()\` to get the current week's journal path
5. Call \`complete_reflect()\` when done to clear Working Memory

## Guidelines

- **Be selective**: Not everything in Working Memory needs to be saved permanently
- **Knowledge notes**: Keep small and focused, dictionary-style, term-based
- **Timeline consolidation**: Transform timeline entries into readable journal summaries, don't just copy verbatim
- **Backfill previous days**: Review timeline dates and add entries under the correct weekday sub-headers (not just today's)
- **Show clear diffs**: User needs to see what's changing before approving
- **Weekly Log structure**: All timeline/work entries go under \`## Log\` header with weekday sub-headers
- **Wait for approval**: Never write files without explicit user approval${
    includePrivate
      ? "\n\n## Private Memory\n\nInclude private notes in this reflection."
      : ""
  }`;

  return {
    messages: [
      {
        role: "user",
        content: {
          type: "text",
          text: promptText,
        },
      },
    ],
  };
}
