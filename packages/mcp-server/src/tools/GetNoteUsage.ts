import { z } from "zod";
import type { McpServer } from "../server.js";
import type { ToolContext } from "../types.js";

/**
 * GetNoteUsage Tool
 *
 * Get usage statistics for notes (for consolidation).
 */
export function registerGetNoteUsage(server: McpServer, context: ToolContext) {
  server.registerTool(
    "get_note_usage",
    {
      title: "Get Note Usage",
      description: "Get usage statistics for notes (for consolidation)",
      inputSchema: {
        notes: z
          .array(z.string())
          .describe("List of note names to get statistics for"),
        period: z
          .enum(["24h", "7d", "30d", "all"])
          .optional()
          .describe("Time period for statistics (default: all)"),
      },
    },
    async ({ notes, period = "all" }) => {
      const { memorySystem, graphIndex } = context;

      const stats = await memorySystem.getNoteUsage(notes, period);

      // Add backlink counts from graph index
      for (const note of notes) {
        const backlinks = graphIndex.getBacklinks(note, false);
        stats[note].backlinks = backlinks.length;
      }

      return {
        content: [
          {
            type: "text",
            text: JSON.stringify(stats, null, 2),
          },
        ],
      };
    }
  );
}
