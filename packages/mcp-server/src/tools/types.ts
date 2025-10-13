/**
 * Type definitions for MCP tools
 */

import { FileOperations } from "../file-operations.js";
import { GraphIndex } from "../graph/graph-index.js";
import { MemorySystem } from "../memory/memory-system.js";
import { ConsolidationManager } from "../memory/consolidation.js";

/**
 * Context passed to all tool handlers
 * Contains shared dependencies and helper functions
 */
export interface ToolContext {
  vaultPath: string;
  fileOps: FileOperations;
  graphIndex: GraphIndex;
  memorySystem: MemorySystem;
  consolidationManager: ConsolidationManager;
  resolveNoteNameToPath: (
    noteName: string,
    includePrivate?: boolean
  ) => string | undefined;
}

/**
 * Tool handler function signature
 * Args are unknown and must be validated with type guards
 */
export type ToolHandler = (
  args: unknown,
  context: ToolContext
) => Promise<ToolResponse>;

/**
 * Tool response format
 */
export interface ToolResponse {
  content: ToolResponseContent[];
  isError?: boolean;
}

/**
 * Tool response content can be text or resource links
 */
export type ToolResponseContent =
  | { type: "text"; text: string }
  | { type: "resource"; resource: ResourceLink };

/**
 * Resource link for MCP
 */
export interface ResourceLink {
  uri: string;
  name: string;
  mimeType: string;
  description?: string;
}

/**
 * Tool definition for MCP server
 */
export interface ToolDefinition {
  name: string;
  description: string;
  inputSchema: {
    type: "object";
    properties: Record<string, unknown>;
    required?: string[];
  };
}

/**
 * Complete MCP tool specification
 */
export interface MCPTool {
  name: string;
  definition: ToolDefinition;
  handler: ToolHandler;
}
