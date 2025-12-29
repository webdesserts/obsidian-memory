import { z } from "zod";
import type { McpServer } from "../server.js";
import type { ToolContext } from "../types.js";
import { extractNoteName, parseWikiLinks } from "@webdesserts/obsidian-memory-core";
import { prepareContentForEmbedding } from "../embeddings/manager.js";
import { logger } from "../utils/logger.js";
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
  const wikiLinks = parseWikiLinks(query) as Array<{ target: string }>;
  const noteReferences = wikiLinks.map((link) => link.target);

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
            "Mixed: 'type safety in [[TypeScript]]' combines note content with text. " +
            "Wiki-links enable graph boosting (connected notes rank higher)."
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
        debug: z
          .boolean()
          .optional()
          .default(false)
          .describe(
            "Show detailed score breakdown (semantic, graph proximity, boost calculation). " +
            "Useful for understanding how results are ranked."
          ),
      },
      annotations: {
        readOnlyHint: true,
        openWorldHint: true,
      },
    },
    async ({ query, includePrivate, topK, minSimilarity, debug }) => {
      logger.info({ group: "Search", query }, "Search started");

      // Wait for cache warmup to complete if still in progress
      if (context.warmupPromise) {
        logger.info({ group: "Search" }, "Waiting for cache warmup to complete");
        await context.warmupPromise;
        logger.info({ group: "Search" }, "Cache warmup complete, proceeding with search");
      }

      // Parse query for note references
      const parsed = parseQueryForNoteReferences(query);
      const hasNoteReferences = parsed.noteReferences.length > 0;
      const hasRemainingText = parsed.remainingText.length > 0;

      if (hasNoteReferences) {
        logger.info({ group: "Search", noteReferences: parsed.noteReferences }, "Found note references");
        if (hasRemainingText) {
          logger.info({ group: "Search", remainingText: parsed.remainingText }, "Remaining text after wiki-links");
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

      logger.info({ group: "Search", candidateCount: candidateNotes.length }, "Searching notes");

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
          logger.error({ group: "Search", notePath, err: error }, "Error reading note");
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
            logger.warn({ group: "Search", noteName }, "Note reference not found");
            continue;
          }

          try {
            const { content, frontmatter } = await context.fileOps.readNote(notePath);
            const preparedContent = prepareContentForEmbedding(noteName, content, frontmatter);
            textsToEmbed.push(preparedContent);
            resolvedNotes.push(noteName);
          } catch (error) {
            logger.error({ group: "Search", notePath, err: error }, "Error reading note reference");
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
        logger.info({ group: "Search", textCount: textsToEmbed.length }, "Encoding query");
        queryEmbedding = await context.embeddingManager.encode(textsToEmbed);
      } else {
        // Simple query - just encode it directly
        queryEmbedding = await context.embeddingManager.encode(query);
      }

      // Search using query embedding
      let results = context.embeddingManager.searchWithVector(
        queryEmbedding,
        embeddings,
        topK * 2 // Get more results before graph boosting to ensure enough after filtering
      );

      // Apply graph boosting if note references are present
      let graphProximity: Map<string, number> | null = null;
      if (hasNoteReferences && resolvedNotes.length > 0) {
        logger.info({ group: "Search", seedCount: resolvedNotes.length }, "Computing graph proximity");
        graphProximity = await context.graphProximityManager.computeMultiSeedProximity(resolvedNotes);

        // Apply multiplicative boost: finalScore = semantic × (1 + graph), capped at 100%
        results = results.map(result => {
          const noteName = extractNoteName(result.filePath);
          const proximity = graphProximity?.get(noteName) || 0;
          const boostedScore = Math.min(1.0, result.similarity * (1 + proximity));

          return {
            filePath: result.filePath,
            similarity: boostedScore,
            // Store original scores for debug output
            _semantic: result.similarity,
            _graph: proximity,
          };
        });

        // Re-sort by boosted scores
        results.sort((a, b) => b.similarity - a.similarity);

        // Trim back to topK after boosting
        results = results.slice(0, topK);

        logger.info({ group: "Search", resultCount: results.length }, "Applied graph boost");
      }

      // Filter by minimum similarity threshold
      const filtered = results.filter(r => r.similarity >= minSimilarity);

      // Format results with note references context and debug info
      return formatResults(filtered, minSimilarity, context, {
        noteReferences: resolvedNotes,
        remainingText: hasRemainingText ? parsed.remainingText : undefined,
        debug,
        graphBoosted: hasNoteReferences && resolvedNotes.length > 0,
      });
    }
  );
}

/**
 * Format search results for display
 */
function formatResults(
  results: Array<{ filePath: string; similarity: number; _semantic?: number; _graph?: number }>,
  minSimilarity: number,
  context: ToolContext,
  queryContext?: {
    noteReferences?: string[];
    remainingText?: string;
    debug?: boolean;
    graphBoosted?: boolean;
  }
): { content: Array<{ type: string; text: string }> } {
  let output = `# Search Results\n\n`;

  // Show what was searched (if note references were used)
  if (queryContext) {
    const { noteReferences, remainingText, debug, graphBoosted } = queryContext;

    if (noteReferences && noteReferences.length > 0) {
      output += `Searching using: `;
      output += noteReferences.map(name => `[[${name}]]`).join(", ");

      if (remainingText) {
        output += `, "${remainingText}"`;
      }

      output += `\n\n`;
    }

    // Show debug info header
    if (debug && graphBoosted) {
      output += `**Debug Mode - Score Breakdown:**\n`;
      output += `- Semantic: Content similarity from averaged embeddings\n`;
      output += `- Graph: Proximity via random walk from ${noteReferences?.length || 0} seed note(s)\n`;
      output += `- Final: semantic × (1 + graph)\n\n`;
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
    output += `- Using wiki-link references like [[Note]] to boost graph-connected results\n`;
    return { content: [{ type: "text", text: output }] };
  }

  output += `Found ${results.length} relevant notes:\n\n`;

  const debug = queryContext?.debug || false;
  const graphBoosted = queryContext?.graphBoosted || false;

  for (let i = 0; i < results.length; i++) {
    const result = results[i];
    const { filePath, similarity } = result;
    const confidence = Math.round(similarity * 100);

    // Extract note name from path
    const noteName = extractNoteName(filePath);

    // Format result with relevance breakdown
    if (graphBoosted && result._semantic !== undefined && result._graph !== undefined) {
      const semanticPct = Math.round(result._semantic * 100);
      const graphPct = Math.round(result._graph * 100);

      output += `${i + 1}. **[[${noteName}]]** (${confidence}% relevant)\n`;
      output += `   - Semantic: ${semanticPct}%\n`;
      output += `   - Graph: ${graphPct}%\n`;

      // Show boost calculation in debug mode
      if (debug) {
        const boost = (1 + result._graph).toFixed(2);
        const uncappedScore = Math.round(result._semantic * (1 + result._graph) * 100);
        const wasCapped = uncappedScore > 100;

        if (wasCapped) {
          output += `   - Boost: ${semanticPct}% × ${boost} = ${uncappedScore}% → capped at ${confidence}%\n`;
        } else {
          output += `   - Boost: ${semanticPct}% × ${boost} = ${confidence}%\n`;
        }
      }

      output += `   - Path: \`${filePath}\`\n\n`;
    } else {
      output += `${i + 1}. **[[${noteName}]]** (${confidence}% relevant)\n`;
      output += `   - Path: \`${filePath}\`\n\n`;
    }
  }

  output += `\n*Use GetNote() to view individual note details*`;

  return {
    content: [{ type: "text", text: output }],
  };
}
