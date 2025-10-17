/**
 * Type definitions for MCP tools
 */

import { z, ZodJSONSchema } from "zod";
import { FileOperations } from "./file-operations.js";
import { GraphIndex } from "./graph/graph-index.js";
import { MemorySystem } from "./memory/memory-system.js";
import { ConsolidationManager } from "./memory/consolidation.js";
import { JSONSchema } from "zod/v4/core";
import { ServerResult, Tool } from "@modelcontextprotocol/sdk/types.js";

/**
 * Context passed to all tool handlers
 * Contains shared dependencies and helper functions
 */
export interface ToolContext {
  vaultPath: string;
  vaultName: string;
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
 * Args are validated by Zod schema and typed accordingly
 */
export type ToolHandler<TArgs extends z.ZodTypeAny> = (
  args: z.infer<TArgs>,
  context: ToolContext
) => Promise<ToolResponse>;

/**
 * Tool response format
 */
export interface ToolResponse {
  content: ToolResponseContent[];
  structuredContent?: Record<string, unknown>;
  isError?: boolean;
}

/**
 * Tool response content can be text, resource links, or embedded resources
 */
export type ToolResponseContent =
  | { type: "text"; text: string }
  | {
      type: "resource_link";
      uri: string;
      name: string;
      mimeType: string;
      description?: string;
    }
  | {
      type: "resource";
      resource: {
        uri: string;
        title: string;
        mimeType: string;
        text?: string | null;
        annotations?: Record<string, unknown>;
      };
    };

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
  inputSchema: JSONSchema.BaseSchema;
}

/**
 * Complete MCP tool specification with Zod schema
 */
export interface MCPTool {
  definition: ToolDefinition;
  handler: (args: unknown, context: ToolContext) => Promise<ToolResponse>;
}
