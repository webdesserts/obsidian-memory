/**
 * All available MCP tools for the Obsidian Memory server
 */

import { MCPTool } from "./types.js";
import { ReadNote } from "./ReadNote.js";
import { UpdateFrontmatter } from "./UpdateFrontmatter.js";
import { GetBacklinks } from "./GetBacklinks.js";
import { GetGraphNeighborhood } from "./GetGraphNeighborhood.js";
import { GetNoteUsage } from "./GetNoteUsage.js";
import { LoadPrivateMemory } from "./LoadPrivateMemory.js";
import { ConsolidateMemory } from "./ConsolidateMemory.js";
import { CompleteConsolidation } from "./CompleteConsolidation.js";
import type { JSONSchema } from "zod/v4/core";

/**
 * Array of all available tools
 * Add new tools here to register them with the MCP server
 */
export const allTools = [
  ReadNote,
  UpdateFrontmatter,
  GetBacklinks,
  GetGraphNeighborhood,
  GetNoteUsage,
  LoadPrivateMemory,
  ConsolidateMemory,
  CompleteConsolidation,
] as const;
