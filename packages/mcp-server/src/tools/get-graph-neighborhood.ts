import { MCPTool, ToolContext, ToolResponseContent } from "./types.js";
import { extractNoteName } from "@obsidian-memory/utils";

/**
 * Type guard for get_graph_neighborhood args
 */
function isGetGraphNeighborhoodArgs(
  args: unknown
): args is { noteName: string; depth?: number; includePrivate?: boolean } {
  return (
    typeof args === "object" &&
    args !== null &&
    "noteName" in args &&
    typeof (args as { noteName: unknown }).noteName === "string"
  );
}

export const getGraphNeighborhoodTool = {
  name: "get_graph_neighborhood",

  definition: {
    name: "get_graph_neighborhood",
    description:
      "Explore notes connected to a note via wiki links (primary discovery tool)",
    inputSchema: {
      type: "object",
      properties: {
        noteName: {
          type: "string",
          description: "The note name to explore from",
        },
        depth: {
          type: "number",
          description:
            "How many hops to explore (1-3 recommended, default: 2)",
        },
        includePrivate: {
          type: "boolean",
          description: "Include private folder notes (default: false)",
        },
      },
      required: ["noteName"],
    },
  },

  async handler(args: unknown, context: ToolContext) {
    if (!isGetGraphNeighborhoodArgs(args)) {
      throw new Error("Invalid arguments: noteName is required");
    }

    const { noteName, depth = 2, includePrivate = false } = args;
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
    const neighborhood = graphIndex.getNeighborhood(
      resolvedNoteName,
      depth,
      includePrivate
    );

    if (neighborhood.size === 0) {
      return {
        content: [
          {
            type: "text",
            text: `No connected notes found for: ${resolvedNoteName} (${resolvedPath})`,
          },
        ],
      };
    }

    // Build text summary
    let summary = `Graph neighborhood for "${resolvedNoteName}" at ${resolvedPath} (depth: ${depth}):\n\n`;

    // Build ResourceLinks grouped by distance
    const resourceLinks: ToolResponseContent[] = [];

    for (let d = 1; d <= depth; d++) {
      const notesAtDistance = Array.from(neighborhood.entries()).filter(
        ([_, info]) => info.distance === d
      );

      if (notesAtDistance.length > 0) {
        summary += `Distance ${d}: ${notesAtDistance.length} note${
          notesAtDistance.length === 1 ? "" : "s"
        }\n`;

        for (const [note, info] of notesAtDistance) {
          const notePath = graphIndex.getNotePath(note) || note;

          // Build description with link information
          let description = `${info.linkType} (distance ${d})`;
          if (info.directLinks.length > 0) {
            description += ` - Links to: ${info.directLinks.join(", ")}`;
          }
          if (info.backlinks.length > 0) {
            description += ` - Linked from: ${info.backlinks.join(", ")}`;
          }

          resourceLinks.push({
            type: "resource",
            resource: {
              uri: `memory://${notePath}`,
              name: note,
              mimeType: "text/markdown",
              description,
            },
          });
        }
      }
    }

    return {
      content: [{ type: "text", text: summary }, ...resourceLinks],
    };
  },
} satisfies MCPTool;
