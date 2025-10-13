import { MCPTool, ToolContext } from "./types.js";

/**
 * Type guard for get_frontmatter args
 */
function isGetFrontmatterArgs(args: unknown): args is { path: string } {
  return (
    typeof args === "object" &&
    args !== null &&
    "path" in args &&
    typeof (args as { path: unknown }).path === "string"
  );
}

export const getFrontmatterTool = {
  name: "get_frontmatter",

  definition: {
    name: "get_frontmatter",
    description: "Get the frontmatter metadata from a note",
    inputSchema: {
      type: "object",
      properties: {
        path: {
          type: "string",
          description: "Path to the note relative to vault root",
        },
      },
      required: ["path"],
    },
  },

  async handler(args: unknown, context: ToolContext) {
    if (!isGetFrontmatterArgs(args)) {
      throw new Error("Invalid arguments: path is required");
    }

    const { path } = args;
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
