import { MCPTool, ToolContext } from "./types.js";
import { extractNoteName } from "@obsidian-memory/utils";

/**
 * Type guard for get_backlinks args
 */
function isGetBacklinksArgs(
  args: unknown
): args is { noteName: string; includePrivate?: boolean } {
  return (
    typeof args === "object" &&
    args !== null &&
    "noteName" in args &&
    typeof (args as { noteName: unknown }).noteName === "string"
  );
}

export const getBacklinksTool = {
  name: "get_backlinks",

  definition: {
    name: "get_backlinks",
    description: "Find all notes that link to a given note",
    inputSchema: {
      type: "object",
      properties: {
        noteName: {
          type: "string",
          description: "The note name (without .md extension)",
        },
        includePrivate: {
          type: "boolean",
          description: "Include links from private folder (default: false)",
        },
      },
      required: ["noteName"],
    },
  },

  async handler(args: unknown, context: ToolContext) {
    if (!isGetBacklinksArgs(args)) {
      throw new Error("Invalid arguments: noteName is required");
    }

    const { noteName, includePrivate = false } = args;
    const { graphIndex, resolveNoteNameToPath } = context;

    // Resolve note name to actual path (handles duplicates)
    const resolvedPath = resolveNoteNameToPath(noteName, includePrivate);
    if (!resolvedPath) {
      return {
        content: [
          { type: "text", text: `Note not found in graph: ${noteName}` },
        ],
      };
    }

    // Use the note name from the resolved path
    const resolvedNoteName = extractNoteName(resolvedPath);
    const backlinks = graphIndex.getBacklinks(resolvedNoteName, includePrivate);

    if (backlinks.length === 0) {
      return {
        content: [
          {
            type: "text",
            text: `No backlinks found for: ${resolvedNoteName} (${resolvedPath})`,
          },
        ],
      };
    }

    // Build ResourceLinks for each backlink
    const resourceLinks = backlinks.map((note) => {
      const notePath = graphIndex.getNotePath(note) || note;
      return {
        type: "resource" as const,
        resource: {
          uri: `memory://${notePath}`,
          name: note,
          mimeType: "text/markdown",
          description: `Links to [[${noteName}]]`,
        },
      };
    });

    // Also include a text summary
    const summary = {
      type: "text" as const,
      text: `Found ${backlinks.length} backlink${
        backlinks.length === 1 ? "" : "s"
      } to "${noteName}"`,
    };

    return {
      content: [summary, ...resourceLinks],
    };
  },
} satisfies MCPTool;
