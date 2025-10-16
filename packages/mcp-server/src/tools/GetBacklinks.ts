import { z } from "zod";
import { ToolContext, MCPTool } from "./types.js";
import { extractNoteName } from "@obsidian-memory/utils";

const Args = z.object({
  noteName: z.string().describe("The note name (without .md extension)"),
  includePrivate: z
    .boolean()
    .optional()
    .describe("Include links from private folder (default: false)"),
});
type Args = z.infer<typeof Args>;

export const GetBacklinks = {
  definition: {
    name: "GetBacklinks",
    description: "Find all notes that link to a given note",
    inputSchema: z.toJSONSchema(Args),
  },

  async handler(args: unknown, context: ToolContext) {
    const { noteName, includePrivate = false } = Args.parse(args);
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
        type: "resource_link" as const,
        uri: `memory://${notePath}`,
        name: note,
        mimeType: "text/markdown",
        description: `Links to [[${noteName}]]`,
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
