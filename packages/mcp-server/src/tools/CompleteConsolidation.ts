import { z } from "zod";
import { ToolContext, MCPTool } from "../types.js";

const Args = z.object({});
type Args = z.infer<typeof Args>;

export const CompleteConsolidation = {
  definition: {
    name: "CompleteConsolidation",
    description:
      "Mark consolidation as complete (deletes Working Memory.md, releases lock)",
    inputSchema: z.toJSONSchema(Args),
  },

  async handler(_args: unknown, context: ToolContext) {
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
  },
} satisfies MCPTool;
