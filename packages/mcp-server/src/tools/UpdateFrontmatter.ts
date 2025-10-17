import { z } from "zod";
import { ToolContext, MCPTool } from "../types.js";

const Args = z.object({
  path: z.string().describe("Path to the note relative to vault root"),
  updates: z.record(z.string(), z.any()).describe("Frontmatter fields to update"),
});
type Args = z.infer<typeof Args>;

export const UpdateFrontmatter = {
  definition: {
    name: "UpdateFrontmatter",
    description: "Update frontmatter metadata in a note",
    inputSchema: z.toJSONSchema(Args),
  },

  async handler(args: unknown, context: ToolContext) {
    const { path, updates } = Args.parse(args);
    const { fileOps } = context;

    await fileOps.updateFrontmatter(path, updates);

    return {
      content: [{ type: "text", text: `Frontmatter updated: ${path}` }],
    };
  },
} satisfies MCPTool;
