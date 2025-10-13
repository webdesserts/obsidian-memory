import { MCPTool, ToolContext } from "./types.js";

/**
 * Type guard for update_frontmatter args
 */
function isUpdateFrontmatterArgs(
  args: unknown
): args is { path: string; updates: Record<string, unknown> } {
  return (
    typeof args === "object" &&
    args !== null &&
    "path" in args &&
    typeof (args as { path: unknown }).path === "string" &&
    "updates" in args &&
    typeof (args as { updates: unknown }).updates === "object" &&
    (args as { updates: unknown }).updates !== null
  );
}

export const updateFrontmatterTool = {
  name: "update_frontmatter",

  definition: {
    name: "update_frontmatter",
    description: "Update frontmatter metadata in a note",
    inputSchema: {
      type: "object",
      properties: {
        path: {
          type: "string",
          description: "Path to the note relative to vault root",
        },
        updates: {
          type: "object",
          description: "Frontmatter fields to update",
        },
      },
      required: ["path", "updates"],
    },
  },

  async handler(args: unknown, context: ToolContext) {
    if (!isUpdateFrontmatterArgs(args)) {
      throw new Error("Invalid arguments: path and updates are required");
    }

    const { path, updates } = args;
    const { fileOps } = context;

    await fileOps.updateFrontmatter(path, updates);

    return {
      content: [{ type: "text", text: `Frontmatter updated: ${path}` }],
    };
  },
} satisfies MCPTool;
