import { z } from "zod";
import { ToolContext, MCPTool } from "../types.js";

const Args = z.object({
  reason: z.string().describe("Reason for loading private memory"),
});
type Args = z.infer<typeof Args>;

export const LoadPrivateMemory = {
  definition: {
    name: "LoadPrivateMemory",
    description: "Load private memory indexes (requires explicit user consent)",
    inputSchema: z.toJSONSchema(Args),
  },

  async handler(args: unknown, context: ToolContext) {
    const { reason } = Args.parse(args);
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
