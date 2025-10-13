import { MCPTool, ToolContext } from "./types.js";

/**
 * Type guard for load_private_memory args
 */
function isLoadPrivateMemoryArgs(args: unknown): args is { reason: string } {
  return (
    typeof args === "object" &&
    args !== null &&
    "reason" in args &&
    typeof (args as { reason: unknown }).reason === "string"
  );
}

export const loadPrivateMemoryTool = {
  name: "load_private_memory",

  definition: {
    name: "load_private_memory",
    description:
      "Load private memory indexes (requires explicit user consent)",
    inputSchema: {
      type: "object",
      properties: {
        reason: {
          type: "string",
          description: "Reason for loading private memory",
        },
      },
      required: ["reason"],
    },
  },

  async handler(args: unknown, context: ToolContext) {
    if (!isLoadPrivateMemoryArgs(args)) {
      throw new Error("Invalid arguments: reason is required");
    }

    const { reason } = args;
    const { memorySystem } = context;

    console.error(`[MemorySystem] Loading private memory: ${reason}`);

    const { longTermIndex, workingMemory } =
      await memorySystem.loadPrivateMemory();

    let response = `# Private Memory Loaded\n\nReason: ${reason}\n\n`;

    if (longTermIndex) {
      response += `## Private Index.md\n\n${longTermIndex}\n\n`;
    } else {
      response += `## Private Index.md\n\nNo private Index.md found\n\n`;
    }

    if (workingMemory) {
      response += `## Private WorkingMemory.md\n\n${workingMemory}\n\n`;
    } else {
      response += `## Private WorkingMemory.md\n\nNo private WorkingMemory.md found\n\n`;
    }

    return {
      content: [{ type: "text", text: response }],
    };
  },
} satisfies MCPTool;
