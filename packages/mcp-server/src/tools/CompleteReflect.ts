import { z } from "zod";
import type { McpServer } from "../server.js";
import type { ToolContext } from "../types.js";

/**
 * CompleteReflect Tool
 *
 * Mark reflection as complete (deletes Log.md and Working Memory.md, releases lock).
 * This is called after reflect workflow successfully consolidates Log and Working Memory.
 */
export function registerCompleteReflect(
  server: McpServer,
  context: ToolContext
) {
  server.registerTool(
    "CompleteReflect",
    {
      title: "Complete Reflect",
      description:
        "Mark reflection as complete (deletes Log.md and Working Memory.md, releases lock)",
      inputSchema: {},
      annotations: {
        readOnlyHint: false,
        destructiveHint: true,
        openWorldHint: false,
      },
    },
    async () => {
      const { fileOps, memorySystem } = context;

      console.error("[Reflect] Completing reflection");

      try {
        const fs = await import("fs/promises");

        // Delete Log.md
        const logPath = fileOps["config"].vaultPath + "/Log.md";
        try {
          await fs.unlink(logPath);
          console.error("[Reflect] Deleted Log.md");
        } catch (error) {
          console.error("[Reflect] No Log.md to delete");
        }

        // Delete Working Memory.md
        const workingMemoryPath = fileOps["config"].vaultPath + "/Working Memory.md";
        try {
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
              text: "Reflection complete! Log.md and Working Memory.md cleared.",
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
