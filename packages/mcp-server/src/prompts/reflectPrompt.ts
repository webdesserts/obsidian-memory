export interface ReflectPromptParams {
  weeklyNotePath: string;
  dayOfWeek: string;
  weekNumber: number;
  includePrivate: boolean;
}

export function buildReflectPrompt(params: ReflectPromptParams): string {
  const { weeklyNotePath, dayOfWeek, weekNumber, includePrivate } = params;

  let prompt = `# Memory Reflection

Review active context and consolidate into permanent notes.

**Core goal:** Maximize token value - keep active work context accessible, move finished work to searchable notes.

## Files to Review

1. **Read Log.md** - Chronological record of session activity with week, day, and timestamps
2. **Read Working Memory.md** - Draft notes (may already be in your context if you've been writing to it)
3. **Read current weekly journal (${weeklyNotePath})** - Check for bloat, compress finished work, extract large topics to dedicated notes
4. **Check project notes** - If loaded in current session, review for consolidation opportunities (extract large RDMPs, archive old decisions)

## Current Week's Journal

Path: ${weeklyNotePath}
**Today is ${dayOfWeek}, week ${weekNumber}**

## Information Lifecycle

Understanding when to keep vs. compress content:

**Active work (keep details in Log.md):**
- Work currently in progress (even if started last week and resumed this week)
- Active debugging: detailed steps tried, what changed, what didn't work
- Work with open PRs: high-level details of approaches/choices (might need to revisit)
- Anything needed to rebuild working context when resuming work

**Shipped/merged work (compress to weekly log):**
- Work that's deployed/merged and no longer being touched
- Compress to high-level summary: WHAT you worked on, not HOW it was done
- Discard implementation details, debugging steps, routine fixes

**Debug detail lifecycle:**
- **Active debugging (not fixed yet):** Keep detailed steps in Log.md
- **Fixed but not shipped (PR open):** Keep high-level details in Log.md
- **Shipped/merged:** Compress to weekly log mention ("debugged X issue"), discard details

**Token efficiency principle:**
- Finite context + focus issues = make every token count
- Log.md should contain only what's needed for current/active work
- Move "done" work to searchable notes, keep "active" work in immediate context

## Consolidation Workflow

### Phase 1: Read & Categorize

Read Log.md, Working Memory.md, current weekly journal, and any project notes loaded this session. Categorize content by destination:

**What to look for:**
- **Log.md & Working Memory:** Primary focus - most consolidation happens here
- **Weekly journal:** Check if this week's log entries are getting too detailed/verbose - compress if needed
- **Project notes:** Look for completed RDMPs that can be archived, large sections that should extract to dedicated notes

**Four consolidation techniques:**
- **Forget** - Remove incorrect, irrelevant, or no longer useful information. Search first to avoid leaving phantom memories in other notes.
- **Compact** - Rewrite to be more concise while preserving essential information. Details that are lost are essentially "forgotten."
- **Migrate** - Move information from one file to another. Co-located in labile notes doesn't mean co-located long-term.
- **Fragment** - Split large notes into smaller focused notes connected by wiki-links when a note exceeds ~2.5k tokens.

1. **Knowledge notes** - Technical facts, APIs, patterns, how things work
   - Term-based, small, focused (dictionary-style)
   - Example: \`knowledge/React Server Components.md\`, \`knowledge/MCP Prompts.md\`
   - Keep these concise - think encyclopedia entries, not articles
   - **Rare for work items** - most work → weekly log or project notes

2. **Project notes** - Design decisions, architecture, project context
   - Deep dives on specific projects
   - Example: \`knowledge/Obsidian Memory Project.md\`
   - Can be longer and more detailed than knowledge notes
   - **RDMP progression:** Working Memory → project note → dedicated RDMP note (as it grows)

3. **Weekly journal Log** - Scannable work summaries from Log.md
   - Add under **"## Log"** header in current week's journal
   - **Timesheet-level detail:** High-level WHAT, not detailed HOW
   - **Compress related work:** Multiple tweaks → "Built X component for TICKET-123"
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
     - Make it readable weeks later without needing the original context
   - **Structure with headings or bold summaries** to make episodes browsable by topic
   - **Preserve work ticket tags** (**LOR-4883**, etc.) from log entries
   - Link to relevant [[Project]] and [[Knowledge]] notes
   - **Most common outcome (90%):** Work details → compress to weekly log, discard details

4. **Discard** - Not valuable long-term
   - Routine fixes, temporary notes, already-documented info
   - Implementation details that don't need preservation
   - Debugging steps for shipped/merged work

### Phase 2: Ask Clarifying Questions (Optional)

**Only ask if there are ambiguities that aren't clarified in the memory files.**

Examples of when to ask:
- "Is work on TICKET-123 still active or has it been shipped/merged?"
- "Should these RDMP details stay in Working Memory or move to a project note?"
- "This debug work - is the issue fixed or still in progress?"

**Don't ask if the memory files already make it clear.**

### Phase 3: Apply Changes

**Read notes before applying changes:**
1. Use \`GetNote()\` to check if notes exist
2. Use \`Read()\` to load existing note content
3. Consider how new content integrates with existing sections
4. Avoid duplicating information already present
5. Look for opportunities to enhance existing sections rather than just appending

**Apply consolidation:**
1. Update or create knowledge/project notes as needed
2. Add compressed summaries to weekly journal under appropriate weekday headers
3. Use \`GetWeeklyNote()\` to get the current week's journal path
4. Use \`Write()\` to save updated notes

**Log.md and Working Memory handling:**
- **Active work (in progress, PR open):** Compact entries in Log.md, keep context in Working Memory
  - Merge related entries to save tokens
  - Preserve state markers (in progress, blocked, tried X/Y)
  - Keep debugging context for unfixed bugs
  - Keep high-level context for open PRs
- **Shipped/merged work:** Extract to weekly log, remove from Log.md and Working Memory
  - Work is done and no longer needs active context tokens
  - Details archived in weekly log
- **Outcome:** Log.md and Working Memory should be **lean and organized, not wiped clean**
  - After consolidation, rewrite both files with compacted active work
  - Remove shipped/merged entries entirely

### Phase 4: Report Summary

Provide a high-level summary of what changed:
- What notes were created or updated
- What was moved to weekly journal
- What was compacted in Log.md, Working Memory, weekly journal, or project notes
- Current state of active work remaining in each file

User can correct if needed.

## Guidelines

- **Be selective**: Not everything needs permanent storage - 90% compresses to weekly log
- **Knowledge notes**: Rare for work items - mostly for broadly useful patterns
- **Project notes**: RDMP/project-specific context, not general knowledge
- **Weekly journal**: Scannable, timesheet-level summaries (WHAT not HOW)
  - If current week's journal is getting too detailed, compress verbose entries
  - Extract large topics (multi-paragraph deep dives) to dedicated notes
- **Project note optimization**: Look for large RDMPs to extract, completed work to archive
  - Apply same "growth pattern": keep adding until too long, then break out and reference
- **Episodic narratives**: Write in "we" voice as shared experiences, not robotic summaries
- **Natural storytelling**: Include thought process, discoveries, and why decisions mattered
- **Map timestamps to weekdays**: Use ISO 8601 timestamps from Log.md
- **Preserve work tags**: Keep work ticket tags (**LOR-4883**, etc.) in weekly journal
- **Compress for scannability**: Related work → single summary, not minute-by-minute
- **Trust your judgment**: Ask only when truly ambiguous, apply changes, report summary
- **After reflection = lean, not empty**: All active context organized and referencing notes, with active work preserved`;

  if (includePrivate) {
    prompt += "\n\n## Private Memory\n\nInclude private notes in this reflection.";
  }

  return prompt;
}
