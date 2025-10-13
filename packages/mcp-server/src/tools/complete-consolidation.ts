import { MCPTool, ToolContext } from "./types.js";

export const completeConsolidationTool = {
  name: "complete_consolidation",

  definition: {
    name: "complete_consolidation",
    description:
      "Mark consolidation as complete (deletes WorkingMemory.md, releases lock)",
    inputSchema: {
      type: "object",
      properties: {},
    },
  },

  async handler(_args: unknown, context: ToolContext) {
    const { consolidationManager } = context;

    console.error("[Consolidation] Completing consolidation");

    await consolidationManager.completeConsolidation();

    return {
      content: [
        {
          type: "text",
          text: "Consolidation complete! WorkingMemory.md deleted, Index.md reloaded.",
        },
      ],
    };
  },
} satisfies MCPTool;
