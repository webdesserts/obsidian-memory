import { z } from "zod";
import type { McpServer } from "../server.js";
import type { ToolContext } from "../types.js";

/**
 * Reflect Tool
 *
 * Review Log.md and Working Memory.md and consolidate content into permanent notes.
 * Returns consolidation instructions that Claude should follow.
 */
export function registerReflect(
  server: McpServer,
  context: ToolContext
) {
  server.registerTool(
    "Reflect",
    {
      title: "Reflect on Log and Working Memory",
      description:
        "Review Log.md and Working Memory.md and consolidate content into permanent notes (knowledge notes, project notes, weekly journal). Returns detailed consolidation instructions.",
      inputSchema: {
        includePrivate: z
          .boolean()
          .optional()
          .describe("Include private notes in reflection (default: false)"),
      },
      annotations: {
        readOnlyHint: false,
        destructiveHint: false,
        openWorldHint: false,
      },
    },
    async ({ includePrivate = false }) => {
      console.error(
        `[Reflect] Triggering reflection (includePrivate: ${includePrivate})`
      );

      // Get current date info
      const now = new Date();
      const weekNumber = getWeekNumber(now);
      const dayOfWeek = getDayOfWeek(now);
      const year = now.getFullYear();
      const weeklyNotePath = `journal/${year}-w${weekNumber
        .toString()
        .padStart(2, "0")}.md`;

      // Generate the reflection instructions inline
      const promptText = `# Memory Reflection

Review Log.md and Working Memory.md and consolidate into permanent notes.

## Files to Review

1. **Read Log.md** - Chronological record of session activity with ISO 8601 timestamps
2. **Read Working Memory.md** - Draft notes (may already be in your context if you've been writing to it)

## Current Week's Journal

Path: ${weeklyNotePath}
**Today is ${dayOfWeek}, week ${weekNumber}**

## Consolidation Workflow

### Phase 1: Read & Categorize

Read Log.md and Working Memory.md. Categorize each piece of content by its destination:

1. **Knowledge notes** - Technical facts, APIs, patterns, how things work
   - Term-based, small, focused (dictionary-style)
   - Example: \`knowledge/React Server Components.md\`, \`knowledge/MCP Prompts.md\`
   - Keep these concise - think encyclopedia entries, not articles

2. **Project notes** - Design decisions, architecture, project context
   - Deep dives on specific projects
   - Example: \`knowledge/Obsidian Memory Project.md\`
   - Can be longer and more detailed than knowledge notes

3. **Weekly journal Log** - Episodic narratives from Log.md
   - Add under **"## Log"** header in current week's journal
   - **Map timestamped Log.md entries to appropriate weekdays**
   - Use ISO 8601 timestamps to determine which day each entry belongs to
   - Today is **${dayOfWeek}**, so entries from today go under \`### ${dayOfWeek}\`
   - Previous days' entries go under \`### Monday\`, \`### Tuesday\`, etc.
   - **Write from "we" perspective - you and the user worked on this together:**
     - "We ran into X bug while working on Y..."
     - "As we dug into the code, we realized..."
     - "This led us to decide..."
   - **Tell the story naturally, like recounting a shared experience:**
     - Include the thought process and what you discovered along the way
     - Connect the dots between what happened and why it mattered
     - Show the journey, not just the destination
     - Make it readable weeks later without needing the original context
   - **Structure with headings or bold summaries** to make episodes browsable by topic
   - **Preserve work ticket tags** (**LOR-4883**, etc.) from log entries
   - Link to relevant [[Project]] and [[Knowledge]] notes

4. **Discard** - Not valuable long-term
   - Routine fixes, temporary notes, already-documented info

### Phase 2: Propose Changes

**IMPORTANT: Read notes before proposing changes**

Before creating proposals for existing notes:
1. Use \`GetNote()\` to check if the note exists
2. Use \`Read()\` to load and understand the current structure
3. Consider how new content integrates with existing sections
4. Avoid duplicating information already present
5. Look for opportunities to enhance existing sections rather than just appending
6. DO NOT create "append-only" proposals - integrate thoughtfully with existing content

For each piece of content you're keeping, show a clear proposal with enough context for review.

**Format:**

\`\`\`
## Weekly Journal: ${weeklyNotePath}
**Action:** Update existing

**Section:** Log → Monday (backfill from previous session)
**Add:**
**Task Management App - Building the todo list component** - We started exploring different approaches for handling nested todos. Initially tried a flat structure with parent IDs, but ran into issues with drag-and-drop reordering. Switched to a tree structure which makes the drag logic much simpler and mirrors how users think about the hierarchy.

**Section:** Log → ${dayOfWeek} (today's entries)
**Add:**
**Task Management App - Persistence strategy** - While implementing auto-save, we realized we needed to debounce the writes to avoid hammering localStorage. As we worked through the implementation, we discovered IndexedDB would be better for our use case since we're storing structured data. Made the switch and it actually simplified a lot of the serialization code we'd been fighting with.

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

1. Use \`GetNote()\` to check if notes exist
2. Use \`Read()\` to load existing note content
3. Use \`Write()\` to save updated notes
4. Use \`GetWeeklyNote()\` to get the current week's journal path
5. Call \`CompleteReflect()\` when done to clear Log.md and Working Memory.md

## Guidelines

- **Be selective**: Not everything in Log or Working Memory needs to be saved permanently
- **Knowledge notes**: Keep small and focused, dictionary-style, term-based
- **Episodic narratives**: Write weekly journal in "we" voice as shared experiences, not robotic summaries
- **Natural storytelling**: Include thought process, discoveries, and why decisions mattered - make it flow
- **Map timestamps to weekdays**: Use ISO 8601 timestamps from Log.md to determine correct weekday sub-headers
- **Preserve work tags**: Keep work ticket tags (**LOR-4883**, etc.) from log entries in weekly journal
- **Show clear diffs**: User needs to see what's changing before approving
- **Weekly Log structure**: All work entries go under \`## Log\` header with weekday sub-headers
- **Wait for approval**: Never write files without explicit user approval${
        includePrivate
          ? "\n\n## Private Memory\n\nInclude private notes in this reflection."
          : ""
      }`;

      return {
        content: [
          {
            type: "text",
            text: promptText,
            annotations: {
              audience: ["assistant"],
              priority: 0.9,
            },
          },
        ],
      };
    }
  );
}

/**
 * Get ISO week number for a date
 * https://en.wikipedia.org/wiki/ISO_week_date
 */
function getWeekNumber(date: Date): number {
  const target = new Date(date.valueOf());
  const dayNumber = (date.getDay() + 6) % 7;
  target.setDate(target.getDate() - dayNumber + 3);
  const firstThursday = target.valueOf();
  target.setMonth(0, 1);
  if (target.getDay() !== 4) {
    target.setMonth(0, 1 + ((4 - target.getDay() + 7) % 7));
  }
  return 1 + Math.ceil((firstThursday - target.valueOf()) / 604800000);
}

/**
 * Get day of week name
 */
function getDayOfWeek(date: Date): string {
  const days = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
  return days[date.getDay()];
}
