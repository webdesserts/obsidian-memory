/**
 * All available MCP tools for the Obsidian Memory server
 */

import { MCPTool } from "./types.js";
import { readNoteTool } from "./read-note.js";
import { getFrontmatterTool } from "./get-frontmatter.js";
import { updateFrontmatterTool } from "./update-frontmatter.js";
import { getBacklinksTool } from "./get-backlinks.js";
import { getGraphNeighborhoodTool } from "./get-graph-neighborhood.js";
import { getNoteUsageTool } from "./get-note-usage.js";
import { loadPrivateMemoryTool } from "./load-private-memory.js";
import { consolidateMemoryTool } from "./consolidate-memory.js";
import { completeConsolidationTool } from "./complete-consolidation.js";

/**
 * Array of all available tools
 * Add new tools here to register them with the MCP server
 */
export const allTools = [
  readNoteTool,
  getFrontmatterTool,
  updateFrontmatterTool,
  getBacklinksTool,
  getGraphNeighborhoodTool,
  getNoteUsageTool,
  loadPrivateMemoryTool,
  consolidateMemoryTool,
  completeConsolidationTool,
] as const satisfies readonly MCPTool[];

// Re-export types for convenience
export type { MCPTool, ToolContext, ToolHandler, ToolResponse } from "./types.js";
