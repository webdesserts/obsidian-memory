import { z } from "zod";
import type { McpServer } from "../server.js";
import type { ToolContext } from "../types.js";
import { logger } from "../utils/logger.js";

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
    "LoadPrivateMemory",
    {
      title: "Load Private Memory",
      description:
        "Load private memory indexes (requires explicit user consent)",
      inputSchema: {
        reason: z.string().describe("Reason for loading private memory"),
      },
      annotations: {
        readOnlyHint: true,
        openWorldHint: false,
      },
    },
    async ({ reason }) => {
      const { memorySystem } = context;

      logger.info({ group: "MemorySystem", reason }, "Loading private memory");

      const { workingMemory } = await memorySystem.loadPrivateMemory();

      let response = `# Private Memory Loaded\n\nReason: ${reason}\n\n`;

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
