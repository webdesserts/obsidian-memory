import { z } from "zod";
import type { McpServer } from "../server.js";
import type { ToolContext } from "../types.js";

/**
 * LoadPrivateMemory Tool
 *
 * Load private memory indexes (requires explicit user consent).
 */
export function registerLoadPrivateMemory(
  server: McpServer,
  context: ToolContext
) {
  server.registerTool(
    "load_private_memory",
    {
      title: "Load Private Memory",
      description:
        "Load private memory indexes (requires explicit user consent)",
      inputSchema: {
        reason: z.string().describe("Reason for loading private memory"),
      },
    },
    async ({ reason }) => {
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
        response += `## Private Working Memory.md\n\n${workingMemory}\n\n`;
      } else {
        response += `## Private Working Memory.md\n\nNo private Working Memory.md found\n\n`;
      }

      return {
        content: [{ type: "text", text: response }],
      };
    }
  );
}
