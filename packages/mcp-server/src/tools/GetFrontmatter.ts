import { z } from "zod";
import { ToolContext, MCPTool } from "./types.js";

const Args = z.object({
  path: z.string().describe("Path to the note relative to vault root"),
});
type Args = z.infer<typeof Args>;

export const GetFrontmatter = {
  definition: {
    name: "GetFrontmatter",
    description: "Get the frontmatter metadata from a note",
    inputSchema: z.toJSONSchema(Args),
  },

  async handler(args: unknown, context: ToolContext) {
    const { path } = Args.parse(args);
    const { fileOps } = context;

    const frontmatter = await fileOps.getFrontmatter(path);

    return {
      content: [
        {
          type: "text",
          text: frontmatter
            ? JSON.stringify(frontmatter, null, 2)
            : "No frontmatter found",
        },
      ],
    };
  },
} satisfies MCPTool;
