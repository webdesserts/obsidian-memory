import { z } from "zod";
import type { McpServer } from "../server.js";
import type { ToolContext } from "../types.js";

/**
 * ConsolidateMemory Tool
 *
 * Trigger memory consolidation (consolidate Working Memory.md into Index.md).
 */
export function registerConsolidateMemory(
  server: McpServer,
  context: ToolContext
) {
  server.registerTool(
    "consolidate_memory",
    {
      title: "Consolidate Memory",
      description:
        "Trigger memory consolidation (consolidate Working Memory.md into Index.md)",
      inputSchema: {
        includePrivate: z
          .boolean()
          .optional()
          .describe("Include private notes in consolidation (default: false)"),
      },
    },
    async ({ includePrivate = false }) => {
      const { consolidationManager } = context;

      console.error(
        `[Consolidation] Triggering consolidation (includePrivate: ${includePrivate})`
      );

      const prompt = await consolidationManager.triggerConsolidation(
        includePrivate
      );

      return {
        content: [{ type: "text", text: prompt }],
      };
    }
  );
}
