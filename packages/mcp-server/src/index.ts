#!/usr/bin/env node

import { basename } from "node:path";
import { McpServer } from "./server.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { ListRootsRequestSchema } from "@modelcontextprotocol/sdk/types.js";
import { FileOperations } from "./file-operations.js";
import { GraphIndex } from "./graph/graph-index.js";
import { MemorySystem } from "./memory/memory-system.js";
import { ConsolidationManager } from "./memory/consolidation.js";
import { resolveNotePath } from "@obsidian-memory/utils";
import { registerAllTools } from "./tools/index.js";
import { ToolContext } from "./types.js";

// Parse command line arguments
const args = process.argv.slice(2);
const vaultPathIndex = args.indexOf("--vault-path");
const vaultPath =
  vaultPathIndex !== -1
    ? args[vaultPathIndex + 1]
    : process.env.OBSIDIAN_VAULT_PATH;

if (!vaultPath) {
  console.error("Error: Vault path is required.");
  console.error("Usage: obsidian-memory-mcp --vault-path <path>");
  console.error("   or: Set OBSIDIAN_VAULT_PATH environment variable");
  process.exit(1);
}

// Derive vault name from path (basename of the vault directory)
const vaultName = basename(vaultPath);

console.error(`[Server] Starting Obsidian Memory MCP Server`);
console.error(`[Server] Vault path: ${vaultPath}`);
console.error(`[Server] Vault name: ${vaultName}`);

// Initialize file operations, graph index, memory system, and consolidation
const fileOps = new FileOperations({ vaultPath });
const graphIndex = new GraphIndex(vaultPath);
const memorySystem = new MemorySystem(vaultPath, fileOps);
const consolidationManager = new ConsolidationManager(
  memorySystem,
  fileOps,
  graphIndex
);

/**
 * Resolve a note name to a specific path using the graph index
 * Handles duplicate note names using priority-based resolution
 */
function resolveNoteNameToPath(
  noteName: string,
  includePrivate: boolean = false
): string | undefined {
  const availablePaths = graphIndex.getAllNotePaths(noteName);
  return resolveNotePath(availablePaths, { includePrivate });
}

// Build tool context with all dependencies
const toolContext = {
  vaultPath,
  vaultName,
  fileOps,
  graphIndex,
  memorySystem,
  consolidationManager,
  resolveNoteNameToPath,
} satisfies ToolContext;

// Create server instance
const server = new McpServer({
  name: "obsidian-memory-mcp",
  version: "0.1.0",
});

// List roots - still using low-level API
server.server.setRequestHandler(ListRootsRequestSchema, async () => {
  return {
    roots: [
      {
        uri: `file://${vaultPath}`,
        name: vaultName,
      },
    ],
  };
});

// Register all tools using high-level API
registerAllTools(server, toolContext);

// Start the server
async function main() {
  // Initialize graph index and memory system
  await graphIndex.initialize();
  await memorySystem.initialize();

  const transport = new StdioServerTransport();
  await server.server.connect(transport);
  console.error("[Server] Obsidian Memory MCP Server running");

  // Clean up on exit
  process.on("SIGINT", async () => {
    console.error("[Server] Shutting down...");
    await graphIndex.dispose();
    process.exit(0);
  });
}

main().catch((error) => {
  console.error("[Server] Fatal error:", error);
  process.exit(1);
});
