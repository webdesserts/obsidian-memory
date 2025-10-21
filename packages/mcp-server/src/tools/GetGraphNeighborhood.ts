import { z } from "zod";
import type { McpServer } from "../server.js";
import type { ToolContext, ToolResponseContent } from "../types.js";
import { extractNoteName } from "@webdesserts/obsidian-memory-utils";

/**
 * GetGraphNeighborhood Tool
 *
 * Explore notes connected to a note via wiki links.
 */
export function registerGetGraphNeighborhood(
  server: McpServer,
  context: ToolContext
) {
  server.registerTool(
    "get_graph_neighborhood",
    {
      title: "Get Graph Neighborhood",
      description:
        "Explore notes connected to a note via wiki links (primary discovery tool)",
      inputSchema: {
        noteName: z.string().describe("The note name to explore from"),
        depth: z
          .number()
          .optional()
          .describe("How many hops to explore (1-3 recommended, default: 2)"),
        includePrivate: z
          .boolean()
          .optional()
          .describe("Include private folder notes (default: false)"),
      },
    },
    async ({ noteName, depth = 2, includePrivate = false }) => {
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
              type: "resource_link",
              uri: `memory:${notePath}`,
              name: note,
              mimeType: "text/markdown",
              description,
            });
          }
        }
      }

      return {
        content: [{ type: "text", text: summary }, ...resourceLinks],
      };
    }
  );
}
