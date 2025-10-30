import { z } from "zod";
import type { McpServer } from "../server.js";
import type { ToolContext } from "../types.js";

/**
 * UpdateFrontmatter Tool
 *
 * Update frontmatter metadata in a note.
 */
export function registerUpdateFrontmatter(
  server: McpServer,
  context: ToolContext
) {
  server.registerTool(
    "UpdateFrontmatter",
    {
      title: "Update Frontmatter",
      description: "Update frontmatter metadata in a note",
      inputSchema: {
        path: z.string().describe("Path to the note relative to vault root"),
        updates: z
          .record(z.string(), z.any())
          .describe("Frontmatter fields to update"),
      },
      annotations: {
        readOnlyHint: false,
        destructiveHint: false,
        idempotentHint: true,
        openWorldHint: false,
      },
    },
    async ({ path, updates }) => {
      const { fileOps } = context;

      await fileOps.updateFrontmatter(path, updates);

      return {
        content: [{ type: "text", text: `Frontmatter updated: ${path}` }],
      };
    }
  );
}
