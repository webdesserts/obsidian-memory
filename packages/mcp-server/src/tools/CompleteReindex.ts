import { z } from "zod";
import type { McpServer } from "../server.js";
import type { ToolContext } from "../types.js";

/**
 * CompleteReindex Tool
 *
 * Mark reindex as complete (reloads Index.md, releases lock).
 */
export function registerCompleteReindex(
  server: McpServer,
  context: ToolContext
) {
  server.registerTool(
    "complete_reindex",
    {
      title: "Complete Reindex",
      description:
        "Mark reindex as complete (reloads Index.md, releases lock)",
      inputSchema: {},
    },
    async () => {
      const { reindexManager } = context;

      console.error("[Reindex] Completing reindex");

      await reindexManager.completeReindex();

      return {
        content: [
          {
            type: "text",
            text: "Reindex complete! Index.md reloaded.",
          },
        ],
      };
    }
  );
}
