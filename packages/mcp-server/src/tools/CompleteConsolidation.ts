import { z } from "zod";
import type { McpServer } from "../server.js";
import type { ToolContext } from "../types.js";

/**
 * CompleteConsolidation Tool
 *
 * Mark consolidation as complete (deletes Working Memory.md, releases lock).
 */
export function registerCompleteConsolidation(
  server: McpServer,
  context: ToolContext
) {
  server.registerTool(
    "complete_consolidation",
    {
      title: "Complete Consolidation",
      description:
        "Mark consolidation as complete (deletes Working Memory.md, releases lock)",
      inputSchema: {},
    },
    async () => {
      const { consolidationManager } = context;

      console.error("[Consolidation] Completing consolidation");

      await consolidationManager.completeConsolidation();

      return {
        content: [
          {
            type: "text",
            text: "Consolidation complete! Working Memory.md deleted, Index.md reloaded.",
          },
        ],
      };
    }
  );
}
