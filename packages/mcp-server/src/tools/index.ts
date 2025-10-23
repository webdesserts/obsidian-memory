/**
 * All available MCP tools for the Obsidian Memory server
 *
 * Tools use server.registerTool() API for clean, type-safe registration
 */

import type { McpServer } from "../server.js";
import type { ToolContext } from "../types.js";
import { registerGetNote } from "./GetNote.js";
import { registerGetWeeklyNote } from "./GetWeeklyNote.js";
import { registerUpdateFrontmatter } from "./UpdateFrontmatter.js";
import { registerGetGraphNeighborhood } from "./GetGraphNeighborhood.js";
import { registerGetNoteUsage } from "./GetNoteUsage.js";
import { registerLoadPrivateMemory } from "./LoadPrivateMemory.js";
import { registerReindex } from "./Reindex.js";
import { registerCompleteReindex } from "./CompleteReindex.js";
import { registerReflect } from "./Reflect.js";
import { registerCompleteReflect } from "./CompleteReflect.js";

/**
 * Register all tools with the MCP server
 */
export function registerAllTools(server: McpServer, context: ToolContext) {
  registerGetNote(server, context);
  registerGetWeeklyNote(server, context);
  registerUpdateFrontmatter(server, context);
  registerGetGraphNeighborhood(server, context);
  registerGetNoteUsage(server, context);
  registerLoadPrivateMemory(server, context);
  registerReindex(server, context);
  registerCompleteReindex(server, context);
  registerReflect(server, context);
  registerCompleteReflect(server, context);
}
