import { FileOperations } from "../file-operations.js";
import { MemorySystem } from "./memory-system.js";
import { GraphIndex } from "../graph/graph-index.js";

/**
 * Reindex workflow - updates Index.md based on knowledge graph changes and access patterns
 */
export class ReindexManager {
  constructor(
    private memorySystem: MemorySystem,
    private fileOps: FileOperations,
    private graphIndex: GraphIndex
  ) {}

  /**
   * Check if reindex should run on SessionStart
   */
  async shouldReindexOnStartup(): Promise<boolean> {
    return await this.memorySystem.shouldConsolidate();
  }

  /**
   * Trigger reindex workflow - updates Index.md based on knowledge graph and access patterns
   */
  async triggerReindex(includePrivate: boolean = false): Promise<string> {
    console.error("[Reindex] Starting reindex workflow");

    // Get current memory state
    const indexMd = this.memorySystem.getIndex() || "";
    const workingMemoryMd = this.memorySystem.getWorkingMemory() || "";

    // Build reindex prompt
    const prompt = this.buildReindexPrompt(
      indexMd,
      workingMemoryMd,
      includePrivate
    );

    // Return the prompt for Claude to process with ultrathink
    return prompt;
  }

  /**
   * Build the reindex prompt for Claude
   */
  private buildReindexPrompt(
    indexMd: string,
    workingMemoryMd: string,
    includePrivate: boolean
  ): string {
    const timestamp = new Date().toISOString();

    let prompt = `# Index Reindex Task

Update Index.md based on knowledge graph changes and access patterns.

**Current Time**: ${new Date().toLocaleString()}
**Reindex Timestamp**: ${timestamp}

## Instructions

1. **Review** the current Index.md and Working Memory.md for context
2. **Use GetNoteUsage()** to check access statistics from the access log
   - **Call without arguments** to get usage for ALL notes in the access log
   - Identify frequently accessed notes that should be promoted to entry points
   - Find stale entries that are rarely accessed
   - Discover notes with high access counts not currently in Index.md
3. **Analyze** the knowledge graph structure
   - Use Search() with wiki-links to explore connections
   - Identify new entry points based on graph topology
4. **Rewrite** Index.md with:
   - Updated entry points organized by domain
   - Promote frequently accessed notes to entry points
   - Remove stale or low-value entries based on access patterns
   - Add notes explaining reindexing decisions
5. **Update frontmatter** with lastReindex timestamp

**Note**: This is just reindexing - don't delete Working Memory.md (use /reflect for that)

## Current Index.md

${indexMd || "*(No Index.md exists yet - create initial structure)*"}

## Current Working Memory.md (for context)

${workingMemoryMd || "*(No Working Memory.md exists yet)*"}

## Output Format

The new Index.md should be a **flat bullet list** of entry point links:

\`\`\`yaml
---
lastReindex: ${timestamp}
---

# Knowledge Index

> Entry points into knowledge graph - organized by domain

## Domain Name

- [[Note Name]] - Brief description of why this is an entry point
- [[Another Note]] - Another brief description
\`\`\`

**Rules**:
- Keep it as a flat bullet list (no nested structures or paragraphs)
- Each link is an entry point into the knowledge graph
- Use access log data to identify important entry points
- Remove rarely accessed or stale entries
- Group by domain headers

**Use ultrathink to analyze access patterns and plan your reindexing strategy.**
`;

    if (includePrivate) {
      prompt += `\n## Private Memory\n\nInclude private notes in this reindex.\n`;
    }

    return prompt;
  }
}
