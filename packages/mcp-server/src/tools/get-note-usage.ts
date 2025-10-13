import { MCPTool, ToolContext } from "./types.js";

/**
 * Type guard for get_note_usage args
 */
function isGetNoteUsageArgs(
  args: unknown
): args is { notes: string[]; period?: "24h" | "7d" | "30d" | "all" } {
  return (
    typeof args === "object" &&
    args !== null &&
    "notes" in args &&
    Array.isArray((args as { notes: unknown }).notes) &&
    (args as { notes: unknown[] }).notes.every(
      (note) => typeof note === "string"
    )
  );
}

export const getNoteUsageTool = {
  name: "get_note_usage",

  definition: {
    name: "get_note_usage",
    description: "Get usage statistics for notes (for consolidation)",
    inputSchema: {
      type: "object",
      properties: {
        notes: {
          type: "array",
          items: { type: "string" },
          description: "List of note names to get statistics for",
        },
        period: {
          type: "string",
          enum: ["24h", "7d", "30d", "all"],
          description: "Time period for statistics (default: all)",
        },
      },
      required: ["notes"],
    },
  },

  async handler(args: unknown, context: ToolContext) {
    if (!isGetNoteUsageArgs(args)) {
      throw new Error("Invalid arguments: notes array is required");
    }

    const { notes, period = "all" } = args;
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
  },
} satisfies MCPTool;
