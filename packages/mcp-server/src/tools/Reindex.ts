import { z } from "zod";
import type { McpServer } from "../server.js";
import type { ToolContext } from "../types.js";

/**
 * Reindex Tool
 *
 * Update Index.md based on knowledge graph changes and access patterns.
 */
export function registerReindex(
  server: McpServer,
  context: ToolContext
) {
  server.registerTool(
    "Reindex",
    {
      title: "Reindex Knowledge Graph",
      description:
        "Update Index.md based on knowledge graph changes and access patterns from the access log",
      inputSchema: {
        includePrivate: z
          .boolean()
          .optional()
          .describe("Include private notes in reindex (default: false)"),
      },
      annotations: {
        readOnlyHint: false,
        destructiveHint: false,
        openWorldHint: false,
      },
    },
    async ({ includePrivate = false }) => {
      const { reindexManager } = context;

      console.error(
        `[Reindex] Triggering reindex (includePrivate: ${includePrivate})`
      );

      const prompt = await reindexManager.triggerReindex(
        includePrivate
      );

      return {
        content: [{ type: "text", text: prompt }],
      };
    }
  );
}
