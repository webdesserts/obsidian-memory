import { z } from "zod";
import type { McpServer } from "../server.js";
import type { ToolContext } from "../types.js";
import { extractNoteName } from "@webdesserts/obsidian-memory-utils";
import { resolveNotePath } from "../path-utils.js";

/**
 * GetNote Tool
 *
 * Get metadata and graph connections for a note.
 * Returns frontmatter, file paths, and links/backlinks.
 */
export function registerGetNote(server: McpServer, context: ToolContext) {
  server.registerTool(
    "GetNote",
    {
      title: "Get Note",
      description:
        "Get metadata and graph connections for a note. Returns frontmatter, file paths, and links/backlinks. Use Read tool to get content.",
      inputSchema: {
        note: z
          .string()
          .describe(
            'Note reference - supports: "memory:Note Name", "memory:knowledge/Note Name", "knowledge/Note Name", "[[Note Name]]"'
          ),
      },
      annotations: {
        readOnlyHint: true,
        openWorldHint: false,
      },
    },
    async ({ note }) => {
      const { graphIndex, vaultPath, vaultName, fileOps } = context;

      // Resolve note reference to vault-relative path
      const notePath = await resolveNotePath({ note, context: { vaultPath } });
      const noteName = extractNoteName(notePath);

      // Check if note exists in graph
      const resolvedPath = graphIndex.getNotePath(noteName);
      if (!resolvedPath) {
        // Note doesn't exist yet
        const filePath = `${vaultPath}/${notePath}.md`;
        const memoryUri = `memory:${notePath}`;
        const obsidianUri = `obsidian://open?vault=${encodeURIComponent(
          vaultName
        )}&file=${encodeURIComponent(notePath)}`;

        return {
          content: [
            {
              type: "text",
              text: `Note not found: ${noteName}

This note doesn't exist yet. You can create it at:
- File path: ${filePath}
- Memory URI: ${memoryUri}
- Obsidian URI: ${obsidianUri}

Use the Write tool with the file path to create this note.`,
            },
          ],
          structuredContent: {
            noteName,
            resolvedPath: notePath,
            filePath,
            memoryUri,
            obsidianUri,
            exists: false,
          },
        };
      }

      // Note exists - get metadata and links
      const filePath = `${vaultPath}/${resolvedPath}.md`;
      const memoryUri = `memory:${resolvedPath}`;
      const obsidianUri = `obsidian://open?vault=${encodeURIComponent(
        vaultName
      )}&file=${encodeURIComponent(resolvedPath)}`;

      // Read frontmatter (don't need full content)
      let frontmatter: Record<string, unknown> | undefined;
      try {
        const result = await fileOps.readNote(`${resolvedPath}.md`);
        frontmatter = result.frontmatter;
      } catch {
        // File might have been deleted between graph lookup and read
        frontmatter = undefined;
      }

      // Get forward links (notes this note links to)
      const forwardLinks = graphIndex.getForwardLinks(noteName);
      const forwardLinkUris = forwardLinks.map((note) => {
        const path = graphIndex.getNotePath(note) || note;
        return `memory:${path}`;
      });

      // Get backlinks (notes that link to this note)
      const backlinks = graphIndex.getBacklinks(noteName, false);
      const backlinkUris = backlinks.map((note) => {
        const path = graphIndex.getNotePath(note) || note;
        return `memory:${path}`;
      });

      // Build structured response
      const structuredContent: Record<string, unknown> = {
        noteName,
        path: resolvedPath,
        filePath,
        memoryUri,
        obsidianUri,
        exists: true,
        links: forwardLinkUris,
        backlinks: backlinkUris,
      };

      if (frontmatter) {
        structuredContent.frontmatter = frontmatter;
      }

      // Build text summary
      const linksSummary =
        forwardLinkUris.length > 0
          ? `\n\nLinks to: ${forwardLinkUris.join(", ")}`
          : "";
      const backlinksSummary =
        backlinkUris.length > 0
          ? `\n\nLinked from: ${backlinkUris.join(", ")}`
          : "";
      const frontmatterSummary = frontmatter
        ? `\n\nFrontmatter: ${Object.keys(frontmatter).join(", ")}`
        : "";

      return {
        content: [
          {
            type: "text",
            text: `Note: ${noteName}
Path: ${resolvedPath}
File: ${filePath}
Memory URI: ${memoryUri}${linksSummary}${backlinksSummary}${frontmatterSummary}

Use Read tool with the file path to view content.
Use Write tool with the file path to edit content.`,
          },
        ],
        structuredContent,
      };
    }
  );
}
