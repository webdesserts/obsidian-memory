#!/usr/bin/env node

import { basename } from "node:path";
import { homedir } from "node:os";
import { McpServer } from "./server.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import { ListRootsRequestSchema } from "@modelcontextprotocol/sdk/types.js";
import { FileOperations } from "./file-operations.js";
import { GraphIndex } from "./graph/graph-index.js";
import { MemorySystem } from "./memory/memory-system.js";
import { ReindexManager } from "./memory/reindex.js";
import { resolveNotePath } from "@webdesserts/obsidian-memory-utils";
import { ToolContext } from "./types.js";

// Tool registrations
import { registerGetNote } from "./tools/GetNote.js";
import { registerGetWeeklyNote } from "./tools/GetWeeklyNote.js";
import { registerGetCurrentDatetime } from "./tools/GetCurrentDatetime.js";
import { registerLog } from "./tools/Log.js";
import { registerUpdateFrontmatter } from "./tools/UpdateFrontmatter.js";
import { registerGetGraphNeighborhood } from "./tools/GetGraphNeighborhood.js";
import { registerGetNoteUsage } from "./tools/GetNoteUsage.js";
import { registerLoadPrivateMemory } from "./tools/LoadPrivateMemory.js";
import { registerReindex } from "./tools/Reindex.js";
import { registerCompleteReindex } from "./tools/CompleteReindex.js";
import { registerReflect } from "./tools/Reflect.js";
import { registerCompleteReflect } from "./tools/CompleteReflect.js";

// Prompt registrations
import { registerReflectPrompt } from "./prompts/Reflect.js";

// Parse command line arguments
const args = process.argv.slice(2);
const vaultPathIndex = args.indexOf("--vault-path");
let vaultPath =
  vaultPathIndex !== -1
    ? args[vaultPathIndex + 1]
    : process.env.OBSIDIAN_VAULT_PATH;

if (!vaultPath) {
  console.error("Error: Vault path is required.");
  console.error("Usage: obsidian-memory-mcp --vault-path <path>");
  console.error("   or: Set OBSIDIAN_VAULT_PATH environment variable");
  process.exit(1);
}

// Expand ~ to home directory
if (vaultPath.startsWith("~/")) {
  vaultPath = vaultPath.replace("~", homedir());
}

// Derive vault name from path (basename of the vault directory)
const vaultName = basename(vaultPath);

console.error(`[Server] Starting Obsidian Memory MCP Server`);
console.error(`[Server] Vault path: ${vaultPath}`);
console.error(`[Server] Vault name: ${vaultName}`);

// Initialize file operations, graph index, memory system, and reindex manager
const fileOps = new FileOperations({ vaultPath });
const graphIndex = new GraphIndex(vaultPath);
const memorySystem = new MemorySystem(vaultPath, fileOps);
const reindexManager = new ReindexManager(
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
  reindexManager,
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

// Register all tools
registerGetNote(server, toolContext);
registerGetWeeklyNote(server, toolContext);
registerGetCurrentDatetime(server, toolContext);
registerLog(server, toolContext);
registerUpdateFrontmatter(server, toolContext);
registerGetGraphNeighborhood(server, toolContext);
registerGetNoteUsage(server, toolContext);
registerLoadPrivateMemory(server, toolContext);
registerReindex(server, toolContext);
registerCompleteReindex(server, toolContext);
registerReflect(server, toolContext);
registerCompleteReflect(server, toolContext);

// Register all prompts
registerReflectPrompt(server, toolContext);

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
