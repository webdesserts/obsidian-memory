import { z } from "zod";
import { ToolContext, MCPTool } from "../types.js";

const Args = z.object({
  includePrivate: z
    .boolean()
    .optional()
    .describe("Include private notes in consolidation (default: false)"),
});
type Args = z.infer<typeof Args>;

export const ConsolidateMemory = {
  definition: {
    name: "ConsolidateMemory",
    description:
      "Trigger memory consolidation (consolidate WorkingMemory.md into Index.md)",
    inputSchema: z.toJSONSchema(Args),
  },

  async handler(args: unknown, context: ToolContext) {
    const { includePrivate = false } = Args.parse(args);
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
  },
} satisfies MCPTool;
