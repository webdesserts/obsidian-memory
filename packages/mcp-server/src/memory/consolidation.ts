import { FileOperations } from "../file-operations.js";
import { MemorySystem } from "./memory-system.js";
import { GraphIndex } from "../graph/graph-index.js";

/**
 * Consolidation workflow
 */
export class ConsolidationManager {
  constructor(
    private memorySystem: MemorySystem,
    private fileOps: FileOperations,
    private graphIndex: GraphIndex
  ) {}

  /**
   * Check if consolidation should run on SessionStart
   */
  async shouldConsolidateOnStartup(): Promise<boolean> {
    if (this.memorySystem.isConsolidating()) {
      console.error("[Consolidation] Already in progress, skipping");
      return false;
    }

    return await this.memorySystem.shouldConsolidate();
  }

  /**
   * Trigger consolidation workflow
   */
  async triggerConsolidation(includePrivate: boolean = false): Promise<string> {
    console.error("[Consolidation] Starting consolidation workflow");

    // Check if already consolidating
    if (this.memorySystem.isConsolidating()) {
      return "Consolidation already in progress";
    }

    // Try to acquire lock
    const lockAcquired = await this.memorySystem.tryAcquireConsolidationLock();
    if (!lockAcquired) {
      return "Consolidation already in progress on another device";
    }

    try {
      // Mark as in progress
      this.memorySystem.startConsolidation();

      // Get current memory state
      const indexMd = this.memorySystem.getIndex() || "";
      const workingMemoryMd = this.memorySystem.getWorkingMemory() || "";

      // Get current timestamp for frontmatter
      const timestamp = new Date().toISOString();

      // Build consolidation prompt
      const prompt = this.buildConsolidationPrompt(
        indexMd,
        workingMemoryMd,
        includePrivate
      );

      // Return the prompt for Claude to process with ultrathink
      return prompt;
    } catch (error) {
      // Clean up on error
      this.memorySystem.endConsolidation();
      await this.memorySystem.releaseConsolidationLock();
      throw error;
    }
  }

  /**
   * Build the consolidation prompt for Claude
   */
  private buildConsolidationPrompt(
    indexMd: string,
    workingMemoryMd: string,
    includePrivate: boolean
  ): string {
    const timestamp = new Date().toISOString();

    let prompt = `# Memory Consolidation Task

You are consolidating short-term working memory into long-term indexed memory.

**Current Time**: ${new Date().toLocaleString()}
**Consolidation Timestamp**: ${timestamp}

## Instructions

1. **Review** the current Index.md and WorkingMemory.md
2. **Analyze** which notes from WorkingMemory.md should be promoted to Index.md
3. **Use get_note_usage()** to check access statistics for notes you're considering
4. **Rewrite** Index.md with:
   - Updated entry points organized by domain
   - Important discoveries from WorkingMemory.md
   - Remove stale or low-value entries
   - Add consolidation notes explaining your decisions
5. **Update frontmatter** with lastConsolidation timestamp
6. **Delete** WorkingMemory.md after consolidation

## Current Index.md

${indexMd || "*(No Index.md exists yet - create initial structure)*"}

## Current WorkingMemory.md

${workingMemoryMd || "*(No WorkingMemory.md exists yet)*"}

## Tools Available

- \`get_note_usage(notes, period)\` - Query access statistics
- \`get_backlinks(note)\` - Find what links to a note
- \`get_graph_neighborhood(note, depth)\` - Explore connections
- \`write_note()\` - Write the new Index.md
- \`update_frontmatter()\` - Update Index.md frontmatter

## Output Format

The new Index.md should be a **flat bullet list** of links with short descriptions:

\`\`\`yaml
---
lastConsolidation: ${timestamp}
---

# Knowledge Index

> Long-term memory - flat list of entry points organized by domain

## Domain Name

- [[Note Name]] - Brief description of why you'd use this link
- [[Another Note]] - Another brief description
\`\`\`

**Rules**:
- Keep it as a flat bullet list (no nested structures or paragraphs)
- Each link should have only a short description of why you'd use it
- Group links by headers (domains, projects, meta) but otherwise keep flat
- Remove links that are no longer relevant or low-value
- Add new important links discovered in WorkingMemory.md

**Use ultrathink to plan your consolidation strategy before writing.**
`;

    if (includePrivate) {
      prompt += `\n## Private Memory\n\nInclude private notes in this consolidation.\n`;
    }

    return prompt;
  }

  /**
   * Complete consolidation (called after Claude writes new Index.md)
   */
  async completeConsolidation(): Promise<void> {
    try {
      // Delete WorkingMemory.md
      const workingMemoryPath = this.fileOps["config"].vaultPath + "/WorkingMemory.md";
      try {
        const fs = await import("fs/promises");
        await fs.unlink(workingMemoryPath);
        console.error("[Consolidation] Deleted WorkingMemory.md");
      } catch (error) {
        console.error("[Consolidation] No WorkingMemory.md to delete");
      }

      // Reload Index.md
      await this.memorySystem.reloadIndex();

      // Release lock
      await this.memorySystem.releaseConsolidationLock();

      console.error("[Consolidation] Consolidation complete");
    } finally {
      // Mark as complete
      this.memorySystem.endConsolidation();
    }
  }

  /**
   * Cancel consolidation (if something goes wrong)
   */
  async cancelConsolidation(): Promise<void> {
    this.memorySystem.endConsolidation();
    await this.memorySystem.releaseConsolidationLock();
    console.error("[Consolidation] Consolidation cancelled");
  }
}
