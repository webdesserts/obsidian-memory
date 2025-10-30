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
    "CompleteReindex",
    {
      title: "Complete Reindex",
      description:
        "Mark reindex as complete (reloads Index.md, releases lock)",
      inputSchema: {},
      annotations: {
        readOnlyHint: false,
        destructiveHint: true,
        openWorldHint: false,
      },
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
