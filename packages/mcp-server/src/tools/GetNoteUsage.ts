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
          .optional()
          .describe("List of note names to get statistics for (omit to get all notes from access log)"),
        period: z
          .enum(["24h", "7d", "30d", "all"])
          .optional()
          .describe("Time period for statistics (default: all)"),
      },
    },
    async ({ notes, period = "all" }) => {
      const { memorySystem, graphIndex } = context;

      const stats = await memorySystem.getNoteUsage(notes, period, graphIndex);

      // Add backlink counts from graph index
      const noteNames = notes ?? Object.keys(stats);
      for (const note of noteNames) {
        if (stats[note]) {
          const backlinks = graphIndex.getBacklinks(note, false);
          stats[note].backlinks = backlinks.length;
        }
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
