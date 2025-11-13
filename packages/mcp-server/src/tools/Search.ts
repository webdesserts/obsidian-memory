import { z } from "zod";
import type { McpServer } from "../server.js";
import type { ToolContext } from "../types.js";
import { extractNoteName, parseWikiLinks } from "@webdesserts/obsidian-memory-utils";
import { prepareContentForEmbedding } from "../embeddings/manager.js";
import path from "path";

/**
 * Parsed query components
 */
interface ParsedQuery {
  /** Note references found in query (wiki-links) */
  noteReferences: string[];
  /** Remaining text after stripping note references */
  remainingText: string;
  /** Original query for logging */
  originalQuery: string;
}

/**
 * Parse query for note references (wiki-links) and extract remaining text
 */
function parseQueryForNoteReferences(query: string): ParsedQuery {
  const wikiLinks = parseWikiLinks(query);
  const noteReferences = wikiLinks.map(link => link.target);

  // Remove all wiki-links from query to get remaining text
  let remainingText = query.replace(/\[\[([^\]]+?)\]\]/g, "");

  // Clean up extra whitespace
  remainingText = remainingText.trim().replace(/\s+/g, " ");

  return {
    noteReferences,
    remainingText,
    originalQuery: query
  };
}

/**
 * Register the Search tool - finds relevant notes using semantic similarity
 *
 * Supports note references via wiki-links:
 * - Single note: [[TypeScript]]
 * - Multiple notes: [[TypeScript]] [[Projects]]
 * - Mixed: type safety in [[TypeScript]] projects
 *
 * @example
 * Search({
 *   query: "How does MCP sampling work?",
 *   includePrivate: false
 * })
 *
 * @example
 * Search({
 *   query: "[[TypeScript]] [[Projects]]",
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
        "Returns similarity-ordered list of potentially relevant notes. " +
        "Supports note references via wiki-links: [[Note Name]]",
      inputSchema: {
        query: z
          .string()
          .describe(
            "The search query - what information are you looking for? " +
            "Supports wiki-links: [[Note]] searches using that note's content. " +
            "Multiple notes: [[TypeScript]] [[Projects]] finds notes similar to BOTH. " +
            "Mixed: 'type safety in [[TypeScript]]' combines note content with text."
          ),
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
          .describe(
            "Minimum similarity threshold 0-1 (default: 0.3). " +
            "0.3 filters out weakly related notes while keeping moderately relevant results. " +
            "Lower values (0.2) include more results, higher values (0.5+) are very strict."
          ),
      },
      annotations: {
        readOnlyHint: true,
        openWorldHint: true,
      },
    },
    async ({ query, includePrivate, topK, minSimilarity }) => {
      console.error(`[Search] Query: "${query}"`);

      // Wait for cache warmup to complete if still in progress
      if (context.warmupPromise) {
        console.error(`[Search] Waiting for cache warmup to complete...`);
        await context.warmupPromise;
        console.error(`[Search] Cache warmup complete, proceeding with search`);
      }

      // Parse query for note references
      const parsed = parseQueryForNoteReferences(query);
      const hasNoteReferences = parsed.noteReferences.length > 0;
      const hasRemainingText = parsed.remainingText.length > 0;

      if (hasNoteReferences) {
        console.error(`[Search] Found ${parsed.noteReferences.length} note references: ${parsed.noteReferences.join(", ")}`);
        if (hasRemainingText) {
          console.error(`[Search] Remaining text: "${parsed.remainingText}"`);
        }
      }

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
          const { content, frontmatter } = await context.fileOps.readNote(notePath);

          // Prepare content with title and frontmatter for embedding (must match warmup logic)
          const preparedContent = prepareContentForEmbedding(noteName, content, frontmatter);

          notesWithContent.push({
            filePath: notePath,
            content: preparedContent,
          });
        } catch (error) {
          console.error(`[Search] Error reading ${notePath}: ${error}`);
        }
      }

      // Encode all notes (uses cache for unchanged files)
      const embeddings = await context.embeddingManager.encodeNotes(
        notesWithContent
      );

      // Build query embedding (with note references if present)
      let queryEmbedding;
      const resolvedNotes: string[] = [];

      if (hasNoteReferences || hasRemainingText) {
        const textsToEmbed: string[] = [];

        // Resolve each note reference and collect content
        for (const noteName of parsed.noteReferences) {
          const notePath = context.graphIndex.getNotePath(noteName);

          if (!notePath) {
            console.error(`[Search] Note reference not found: ${noteName}`);
            continue;
          }

          try {
            const { content, frontmatter } = await context.fileOps.readNote(notePath);
            const preparedContent = prepareContentForEmbedding(noteName, content, frontmatter);
            textsToEmbed.push(preparedContent);
            resolvedNotes.push(noteName);
          } catch (error) {
            console.error(`[Search] Error reading note reference ${notePath}: ${error}`);
          }
        }

        // Add remaining text if present
        if (hasRemainingText) {
          textsToEmbed.push(parsed.remainingText);
        }

        if (textsToEmbed.length === 0) {
          return {
            content: [{
              type: "text",
              text: "# Search Error\n\nNo valid note references found and no remaining text to search."
            }]
          };
        }

        // Encode (averaging handled by encode method if multiple texts)
        console.error(`[Search] Encoding ${textsToEmbed.length} text piece(s)...`);
        queryEmbedding = await context.embeddingManager.encode(textsToEmbed);
      } else {
        // Simple query - just encode it directly
        queryEmbedding = await context.embeddingManager.encode(query);
      }

      // Search using query embedding
      const results = context.embeddingManager.searchWithVector(
        queryEmbedding,
        embeddings,
        topK
      );

      // Filter by minimum similarity threshold
      const filtered = results.filter(r => r.similarity >= minSimilarity);

      // Format results with note references context
      return formatResults(filtered, minSimilarity, context, {
        noteReferences: resolvedNotes,
        remainingText: hasRemainingText ? parsed.remainingText : undefined
      });
    }
  );
}

/**
 * Format search results for display
 */
function formatResults(
  results: Array<{ filePath: string; similarity: number }>,
  minSimilarity: number,
  context: ToolContext,
  queryContext?: {
    noteReferences?: string[];
    remainingText?: string;
  }
): { content: Array<{ type: string; text: string }> } {
  let output = `# Search Results\n\n`;

  // Show what was searched (if note references were used)
  if (queryContext) {
    const { noteReferences, remainingText } = queryContext;

    if (noteReferences && noteReferences.length > 0) {
      output += `Searching using: `;
      output += noteReferences.map(name => `[[${name}]]`).join(", ");

      if (remainingText) {
        output += `, "${remainingText}"`;
      }

      output += `\n\n`;
    }
  }

  if (results.length === 0) {
    output += `No notes found with similarity >= ${Math.round(minSimilarity * 100)}%\n\n`;
    output += `**Similarity Ranges:**\n`;
    output += `- 0.7+ = Very similar (paraphrases, same topic)\n`;
    output += `- 0.5-0.7 = Related topics\n`;
    output += `- 0.3-0.5 = Weak relation\n`;
    output += `- <0.3 = Mostly unrelated\n\n`;
    output += `**Try:**\n`;
    output += `- Using different search terms\n`;
    output += `- Lowering minSimilarity threshold (currently ${minSimilarity})\n`;
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
