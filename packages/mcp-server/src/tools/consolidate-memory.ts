import { MCPTool, ToolContext } from "./types.js";

/**
 * Type guard for consolidate_memory args
 */
function isConsolidateMemoryArgs(
  args: unknown
): args is { includePrivate?: boolean } {
  // includePrivate is optional, so we just need to check if args is an object
  return typeof args === "object" && args !== null;
}

export const consolidateMemoryTool = {
  name: "consolidate_memory",

  definition: {
    name: "consolidate_memory",
    description:
      "Trigger memory consolidation (consolidate WorkingMemory.md into Index.md)",
    inputSchema: {
      type: "object",
      properties: {
        includePrivate: {
          type: "boolean",
          description:
            "Include private notes in consolidation (default: false)",
        },
      },
    },
  },

  async handler(args: unknown, context: ToolContext) {
    if (!isConsolidateMemoryArgs(args)) {
      throw new Error("Invalid arguments");
    }

    const includePrivate =
      "includePrivate" in args &&
      typeof args.includePrivate === "boolean"
        ? args.includePrivate
        : false;

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
