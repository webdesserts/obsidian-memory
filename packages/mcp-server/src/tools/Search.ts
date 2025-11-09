import { z } from "zod";
import type { McpServer } from "../server.js";
import type { ToolContext } from "../types.js";
import { extractNoteName } from "@webdesserts/obsidian-memory-utils";
import path from "path";

/**
 * Register the Search tool - finds relevant notes using semantic similarity
 *
 * @example
 * Search({
 *   query: "How does MCP sampling work?",
 *   includePrivate: false
 * })
 */
export function registerSearch(server: McpServer, context: ToolContext) {
  server.registerTool(
    "Search",
    {
      description:
        "Search for relevant notes using semantic similarity. " +
        "Encodes the query and compares it against all note embeddings. " +
        "Returns similarity-ordered list of potentially relevant notes.",
      inputSchema: {
        query: z
          .string()
          .describe("The search query - what information are you looking for?"),
        includePrivate: z
          .boolean()
          .optional()
          .default(false)
          .describe(
            "Whether to include private notes in search. Requires explicit user consent."
          ),
        topK: z
          .number()
          .optional()
          .default(10)
          .describe("Number of results to return (default: 10)"),
        minSimilarity: z
          .number()
          .optional()
          .default(0.3)
          .describe("Minimum similarity threshold 0-1 (default: 0.3)"),
      },
      annotations: {
        readOnlyHint: true,
        openWorldHint: true,
      },
    },
    async ({ query, includePrivate, topK, minSimilarity }) => {
      console.error(`[Search] Query: "${query}"`);

      // Get all markdown files in vault
      const allFiles = context.graphIndex.getAllNotes();

      // Filter out private notes if needed
      const candidateNotes = includePrivate
        ? allFiles
        : allFiles.filter((noteName: string) => {
            const notePath = context.graphIndex.getNotePath(noteName);
            return notePath && !notePath.startsWith("private/");
          });

      console.error(`[Search] Searching ${candidateNotes.length} notes...`);

      // Load file contents for all candidate notes
      const notesWithContent: Array<{ filePath: string; content: string }> = [];

      for (const noteName of candidateNotes) {
        const notePath = context.graphIndex.getNotePath(noteName);
        if (!notePath) continue;

        try {
          const { content } = await context.fileOps.readNote(notePath);
          notesWithContent.push({
            filePath: notePath,
            content,
          });
        } catch (error) {
          console.error(`[Search] Error reading ${notePath}: ${error}`);
        }
      }

      // Encode all notes (uses cache for unchanged files)
      const embeddings = await context.embeddingManager.encodeNotes(
        notesWithContent
      );

      // Search using query
      const results = await context.embeddingManager.search(
        query,
        embeddings,
        topK
      );

      // Filter by minimum similarity threshold
      const filtered = results.filter(r => r.similarity >= minSimilarity);

      // Format results
      return formatResults(filtered, minSimilarity, context);
    }
  );
}

/**
 * Format search results for display
 */
function formatResults(
  results: Array<{ filePath: string; similarity: number }>,
  minSimilarity: number,
  context: ToolContext
): { content: Array<{ type: string; text: string }> } {
  let output = `# Search Results\n\n`;

  if (results.length === 0) {
    output += `No notes found with similarity >= ${Math.round(minSimilarity * 100)}%\n\n`;
    output += `Try:\n`;
    output += `- Using different search terms\n`;
    output += `- Lowering minSimilarity threshold\n`;
    output += `- Using GetGraphNeighborhood() to explore from known notes\n`;
    return { content: [{ type: "text", text: output }] };
  }

  output += `Found ${results.length} relevant notes:\n\n`;

  for (let i = 0; i < results.length; i++) {
    const { filePath, similarity } = results[i];
    const confidence = Math.round(similarity * 100);

    // Extract note name from path
    const noteName = extractNoteName(filePath);

    // Get link statistics
    const forwardLinks = context.graphIndex.getForwardLinks(noteName).length;
    const backlinks = context.graphIndex.getBacklinks(noteName).length;

    output += `${i + 1}. **[[${noteName}]]** (${confidence}% similar)\n`;
    output += `   - Path: \`${filePath}\`\n`;
    output += `   - Links: ${forwardLinks} forward, ${backlinks} backlinks\n\n`;
  }

  output += `\n*Use GetNote() to view individual note details*`;

  return {
    content: [{ type: "text", text: output }],
  };
}
