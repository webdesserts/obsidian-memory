import { z } from "zod";
import type { McpServer } from "../server.js";
import type { ToolContext } from "../types.js";
import {
  extractLinkedNotes,
  extractNoteName,
  renderGraphTree,
  type NeighborhoodNode,
} from "@webdesserts/obsidian-memory-utils";
import { LLMManager } from "../llm/manager.js";
import type { SearchDecision } from "../llm/types.js";
import { debugLog } from "../utils/logger.js";

/**
 * Register the Search tool - iteratively explores knowledge graph using local LLM
 * to find relevant notes based on a query
 *
 * @example
 * Search({
 *   query: "How does MCP sampling work?",
 *   startingPoints: ["MCP Servers"],
 *   includePrivate: false
 * })
 */
export function registerSearch(server: McpServer, context: ToolContext) {
  server.registerTool(
    "Search",
    {
      description:
        "Search for relevant notes by iteratively exploring the knowledge graph. " +
        "Uses local LLM to make strategic decisions about which notes to explore based on titles and metadata. " +
        "Returns confidence-ordered list of potentially relevant notes.",
      inputSchema: {
        query: z
          .string()
          .describe("The search query - what information are you looking for?"),
        startingPoints: z
          .array(z.string())
          .optional()
          .describe(
            "Optional list of note names to start exploration from. If not provided, starts from Index.md entry points."
          ),
        includePrivate: z
          .boolean()
          .optional()
          .default(false)
          .describe(
            "Whether to include private notes in search. Requires explicit user consent."
          ),
      },
      annotations: {
        readOnlyHint: true,
        openWorldHint: true,
      },
    },
    async ({ query, startingPoints, includePrivate }) => {
      // Main search loop
      const MAX_ITERATIONS = 10;
      const explored = new Set<string>();
      const allConfidenceScores = new Map<string, number>();

      let currentFrontier = await getStartingPoints(
        startingPoints,
        context,
        includePrivate
      );

      for (let iteration = 0; iteration < MAX_ITERATIONS; iteration++) {
        // Collect neighborhoods for current frontier
        let neighborhoods: Map<string, NeighborhoodNode>;

        // For iteration 0, show starting points with their neighbors
        if (iteration === 0) {
          // First, collect neighbors of the starting points (gives us distance 1-2 nodes)
          neighborhoods = collectNeighborhoods(
            currentFrontier,
            explored,
            context,
            includePrivate || false
          );

          // Then add the starting points themselves at distance 1 (excluding Index)
          // This ensures they appear in the visualization with [N unexplored] counts
          for (const noteName of currentFrontier) {
            if (noteName !== "Index" && !neighborhoods.has(noteName)) {
              neighborhoods.set(noteName, {
                distance: 1,
                linkType: "forward" as const,
                directLinks: context.graphIndex.getForwardLinks(noteName),
                backlinks: context.graphIndex.getBacklinks(noteName),
              });
            }
          }
        } else {
          // For subsequent iterations, show neighbors of the frontier
          neighborhoods = collectNeighborhoods(
            currentFrontier,
            explored,
            context,
            includePrivate || false
          );
        }

        if (neighborhoods.size === 0) {
          debugLog("[Search] No more nodes to explore");
          break; // No more nodes to explore
        }

        // Create visualization for LLM
        const visualization = createVisualization(
          iteration === 0 ? ["Initial Nodes"] : currentFrontier,
          neighborhoods,
          context
        );

        // Request LLM decision
        const decision = await requestLLMDecision(
          query,
          visualization,
          iteration,
          MAX_ITERATIONS
        );

        // Merge confidence scores
        for (const [note, score] of decision.confidenceScores.entries()) {
          allConfidenceScores.set(note, score);
        }

        if (decision.shouldStop || decision.nodesToExplore.length === 0) {
          break;
        }

        // Update frontier and explored set
        currentFrontier.forEach((note) => explored.add(note));
        currentFrontier = decision.nodesToExplore;
      }

      // Format and return results
      return formatResults(explored, allConfidenceScores, context);
    }
  );
}

/**
 * Get starting points for search exploration
 */
async function getStartingPoints(
  startingPoints: string[] | undefined,
  context: ToolContext,
  includePrivate: boolean
): Promise<string[]> {
  // If startingPoints explicitly provided, use them
  if (startingPoints && startingPoints.length > 0) {
    const resolved: string[] = [];
    for (const noteName of startingPoints) {
      const path = context.resolveNoteNameToPath(noteName, includePrivate);
      if (path) {
        resolved.push(extractNoteName(path));
      } else {
        console.warn(`[Search] Starting point not found: ${noteName}`);
      }
    }
    return resolved;
  }

  // Otherwise, extract entry points from Index.md
  const indexContent = context.memorySystem.getIndex();
  if (!indexContent) {
    console.warn("[Search] No Index.md found, starting with empty set");
    return [];
  }

  const entryPoints = extractLinkedNotes(indexContent);

  const resolved: string[] = [];

  for (const noteName of entryPoints) {
    const path = context.resolveNoteNameToPath(noteName, includePrivate);
    if (path) {
      resolved.push(extractNoteName(path));
    }
  }

  return resolved;
}

/**
 * Collect neighborhoods for all notes in current frontier
 * Merges neighborhoods from multiple frontier nodes into a single view
 */
function collectNeighborhoods(
  frontier: string[],
  explored: Set<string>,
  context: ToolContext,
  includePrivate: boolean
): Map<string, NeighborhoodNode> {
  const combined = new Map<string, NeighborhoodNode>();

  // Get neighborhood for each frontier node
  for (const noteName of frontier) {
    const neighborhood = context.graphIndex.getNeighborhood(
      noteName,
      2,
      includePrivate
    );

    // Merge into combined map
    for (const [note, info] of neighborhood.entries()) {
      // Skip already explored notes
      if (explored.has(note) || frontier.includes(note)) {
        continue;
      }

      // If note already exists in combined, merge with preference for lower distance
      const existing = combined.get(note);
      if (existing) {
        if (info.distance < existing.distance) {
          // Use shorter distance path
          combined.set(note, info);
        } else if (info.distance === existing.distance) {
          // Same distance - merge link types
          const mergedLinkType =
            existing.linkType === "both" || info.linkType === "both"
              ? "both"
              : existing.linkType === info.linkType
              ? existing.linkType
              : "both";

          combined.set(note, {
            distance: info.distance,
            linkType: mergedLinkType,
            directLinks: Array.from(
              new Set([...existing.directLinks, ...info.directLinks])
            ),
            backlinks: Array.from(
              new Set([...existing.backlinks, ...info.backlinks])
            ),
          });
        }
      } else {
        // New note, add it
        combined.set(note, info);
      }
    }
  }

  return combined;
}

/**
 * Create ASCII tree visualization for LLM
 */
function createVisualization(
  frontier: string[],
  neighborhoods: Map<string, NeighborhoodNode>,
  context: ToolContext
): string {
  // Get duplicate notes set
  const duplicates = getDuplicateNotes(neighborhoods, context);

  // Create getPathForNote function
  const getPathForNote = (name: string) => {
    return context.graphIndex.getNotePath(name);
  };

  // If multiple frontier nodes, show as single exploration level
  const startNode =
    frontier.length === 1
      ? frontier[0]
      : `${frontier.length} starting points`;

  return renderGraphTree(startNode, neighborhoods, duplicates, getPathForNote);
}

/**
 * Get set of note names that have duplicates (multiple paths)
 * Checks notes in the provided neighborhood
 */
function getDuplicateNotes(
  neighborhood: Map<string, NeighborhoodNode>,
  context: ToolContext
): Set<string> {
  const duplicates = new Set<string>();

  // Check each note in neighborhood for multiple paths
  for (const noteName of neighborhood.keys()) {
    const paths = context.graphIndex.getAllNotePaths(noteName);
    if (paths.length > 1) {
      duplicates.add(noteName);
    }
  }

  return duplicates;
}

/**
 * Request LLM decision about which nodes to explore next
 */
async function requestLLMDecision(
  query: string,
  visualization: string,
  iteration: number,
  maxIterations: number
): Promise<SearchDecision> {
  try {
    const manager = await LLMManager.getInstance();
    return await manager.generateSearchDecision(
      query,
      visualization,
      iteration,
      maxIterations
    );
  } catch (error) {
    debugLog(`[Search] Error: ${error}`);

    // Return safe default decision on error
    return {
      nodesToExplore: [],
      shouldStop: true,
      confidenceScores: new Map(),
    };
  }
}

/**
 * Format search results as confidence-ordered list
 */
function formatResults(
  explored: Set<string>,
  confidenceScores: Map<string, number>,
  context: ToolContext
): { content: Array<{ type: string; text: string }> } {
  // Convert explored set to array with scores
  const results = Array.from(explored).map((noteName) => ({
    noteName,
    score: confidenceScores.get(noteName) || 0.5, // Default confidence
    path: context.graphIndex.getNotePath(noteName),
    forwardLinks: context.graphIndex.getForwardLinks(noteName).length,
    backlinks: context.graphIndex.getBacklinks(noteName).length,
  }));

  // Sort by confidence score (descending)
  results.sort((a, b) => b.score - a.score);

  // Filter to only show results with >= 40% confidence
  const filtered = results.filter((r) => r.score >= 0.4);

  // Format as text
  let output = `# Search Results\n\n`;
  output += `Found ${filtered.length} potentially relevant notes:\n\n`;

  for (let i = 0; i < filtered.length; i++) {
    const { noteName, score, path, forwardLinks, backlinks } = filtered[i];
    const confidence = Math.round(score * 100);

    output += `${i + 1}. **[[${noteName}]]** (${confidence}% confidence)\n`;
    if (path) {
      output += `   - Path: \`${path}\`\n`;
    }
    output += `   - Links: ${forwardLinks} forward, ${backlinks} backlinks\n\n`;
  }

  output += `\n*Use GetNote() to view individual note details*`;

  return {
    content: [{ type: "text", text: output }],
  };
}
