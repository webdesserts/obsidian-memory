import { z } from "zod";
import type { McpServer } from "../server.js";
import type { ToolContext } from "../types.js";

/**
 * CompleteReflect Tool
 *
 * Mark reflection as complete (deletes Working Memory.md, releases lock).
 * This is called after reflect workflow successfully consolidates Working Memory.
 */
export function registerCompleteReflect(
  server: McpServer,
  context: ToolContext
) {
  server.registerTool(
    "complete_reflect",
    {
      title: "Complete Reflect",
      description:
        "Mark reflection as complete (deletes Working Memory.md, releases lock)",
      inputSchema: {},
    },
    async () => {
      const { fileOps, memorySystem } = context;

      console.error("[Reflect] Completing reflection");

      try {
        // Delete Working Memory.md
        const workingMemoryPath = fileOps["config"].vaultPath + "/Working Memory.md";
        try {
          const fs = await import("fs/promises");
          await fs.unlink(workingMemoryPath);
          console.error("[Reflect] Deleted Working Memory.md");
        } catch (error) {
          console.error("[Reflect] No Working Memory.md to delete");
        }

        // Release consolidation lock (shared with reindex)
        await memorySystem.releaseConsolidationLock();

        // Mark as complete
        memorySystem.endConsolidation();

        console.error("[Reflect] Reflection complete");

        return {
          content: [
            {
              type: "text",
              text: "Reflection complete! Working Memory.md cleared.",
            },
          ],
        };
      } catch (error) {
        // Ensure cleanup on error
        memorySystem.endConsolidation();
        await memorySystem.releaseConsolidationLock();
        throw error;
      }
    }
  );
}
