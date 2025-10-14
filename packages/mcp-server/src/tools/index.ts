/**
 * All available MCP tools for the Obsidian Memory server
 */

import { MCPTool } from "./types.js";
import { readNote } from "./read-note.js";
import { getFrontmatter } from "./get-frontmatter.js";
import { updateFrontmatter } from "./update-frontmatter.js";
import { getBacklinks } from "./get-backlinks.js";
import { getGraphNeighborhood } from "./get-graph-neighborhood.js";
import { getNoteUsage } from "./get-note-usage.js";
import { loadPrivateMemory } from "./load-private-memory.js";
import { consolidateMemory } from "./consolidate-memory.js";
import { completeConsolidation } from "./complete-consolidation.js";
import type { JSONSchema } from "zod/v4/core";

/**
 * Array of all available tools
 * Add new tools here to register them with the MCP server
 */
export const allTools = [
  readNote,
  getFrontmatter,
  updateFrontmatter,
  getBacklinks,
  getGraphNeighborhood,
  getNoteUsage,
  loadPrivateMemory,
  consolidateMemory,
  completeConsolidation,
] as const;
