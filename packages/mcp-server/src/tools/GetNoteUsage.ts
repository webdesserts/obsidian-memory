import { z } from "zod";
import { ToolContext, MCPTool } from "../types.js";

const Args = z.object({
  notes: z.array(z.string()).describe("List of note names to get statistics for"),
  period: z
    .enum(["24h", "7d", "30d", "all"])
    .optional()
    .describe("Time period for statistics (default: all)"),
});
type Args = z.infer<typeof Args>;

export const GetNoteUsage = {
  definition: {
    name: "GetNoteUsage",
    description: "Get usage statistics for notes (for consolidation)",
    inputSchema: z.toJSONSchema(Args),
  },

  async handler(args: unknown, context: ToolContext) {
    const { notes, period = "all" } = Args.parse(args);
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
