#!/usr/bin/env node

import { basename } from "node:path";
import { homedir } from "node:os";
import { McpServer } from "./server.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import {
  ListRootsRequestSchema,
  ListResourcesRequestSchema,
} from "@modelcontextprotocol/sdk/types.js";
import { FileOperations } from "./file-operations.js";
import { GraphIndex } from "./graph/graph-index.js";
import { MemorySystem } from "./memory/memory-system.js";
import { ReindexManager } from "./memory/reindex.js";
import { EmbeddingManager } from "./embeddings/manager.js";
import { GraphProximityManager } from "./embeddings/graph-manager.js";
import { resolveNotePath } from "@webdesserts/obsidian-memory-utils";
import { ToolContext } from "./types.js";
import path from "path";
import { debugLog } from "./utils/logger.js";

// Global error handlers - log all uncaught errors to debug.log
process.on("uncaughtException", (error) => {
  debugLog(`[FATAL] Uncaught exception: ${error}`);
  debugLog(`[FATAL] Stack trace: ${error.stack}`);
  process.exit(1);
});

process.on("unhandledRejection", (reason, promise) => {
  debugLog(`[FATAL] Unhandled rejection at: ${promise}, reason: ${reason}`);
  if (reason instanceof Error) {
    debugLog(`[FATAL] Stack trace: ${reason.stack}`);
  }
  process.exit(1);
});

// Tool registrations
import { registerGetNote } from "./tools/GetNote.js";
import { registerGetWeeklyNote } from "./tools/GetWeeklyNote.js";
import { registerGetCurrentDatetime } from "./tools/GetCurrentDatetime.js";
import { registerLog } from "./tools/Log.js";
import { registerRemember } from "./tools/Remember.js";
import { registerUpdateFrontmatter } from "./tools/UpdateFrontmatter.js";
import { registerGetGraphNeighborhood } from "./tools/GetGraphNeighborhood.js";
import { registerGetNoteUsage } from "./tools/GetNoteUsage.js";
import { registerLoadPrivateMemory } from "./tools/LoadPrivateMemory.js";
import { registerReindex } from "./tools/Reindex.js";
import { registerCompleteReindex } from "./tools/CompleteReindex.js";
import { registerReflect } from "./tools/Reflect.js";
import { registerCompleteReflect } from "./tools/CompleteReflect.js";
import { registerSearch } from "./tools/Search.js";

// Parse command line arguments
const args = process.argv.slice(2);
const vaultPathIndex = args.indexOf("--vault-path");
let vaultPathRaw =
  vaultPathIndex !== -1
    ? args[vaultPathIndex + 1]
    : process.env.OBSIDIAN_VAULT_PATH;

if (!vaultPathRaw) {
  console.error("Error: Vault path is required.");
  console.error("Usage: obsidian-memory-mcp --vault-path <path>");
  console.error("   or: Set OBSIDIAN_VAULT_PATH environment variable");
  process.exit(1);
}

// Expand ~ to home directory
const vaultPath = vaultPathRaw.startsWith("~/")
  ? vaultPathRaw.replace("~", homedir())
  : vaultPathRaw;

// Derive vault name from path (basename of the vault directory)
const vaultName = basename(vaultPath);

console.error(`[Server] Starting Obsidian Memory MCP Server`);
console.error(`[Server] Vault path: ${vaultPath}`);
console.error(`[Server] Vault name: ${vaultName}`);

// Initialize file operations, graph index, memory system, reindex manager, and embedding manager
const fileOps = new FileOperations({ vaultPath });
const graphIndex = new GraphIndex(vaultPath);
const memorySystem = new MemorySystem(vaultPath, fileOps);
const reindexManager = new ReindexManager(
  memorySystem,
  fileOps,
  graphIndex
);

// EmbeddingManager and GraphProximityManager will be initialized in main() after graphIndex is ready
let embeddingManager: EmbeddingManager;
let graphProximityManager: GraphProximityManager;

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
// Note: embeddingManager and graphProximityManager will be initialized in main() and added via getters
const toolContext = {
  vaultPath,
  vaultName,
  fileOps,
  graphIndex,
  memorySystem,
  reindexManager,
  get embeddingManager() {
    if (!embeddingManager) {
      throw new Error("EmbeddingManager not initialized yet");
    }
    return embeddingManager;
  },
  get graphProximityManager() {
    if (!graphProximityManager) {
      throw new Error("GraphProximityManager not initialized yet");
    }
    return graphProximityManager;
  },
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

// List resources - expose key vault files and folders
server.server.setRequestHandler(ListResourcesRequestSchema, async () => {
  return {
    resources: [
      {
        name: "Index",
        uri: "memory:Index",
        title: "Commonly Used and Important Notes File",
        description:
          "Auto-loaded at session start. Contains curated links organized by domain (Projects, Programming Languages, Technical, etc.)",
        mimeType: "text/markdown",
        annotations: {
          audience: ["assistant"],
          priority: 1.0,
        },
      },
      {
        name: "Log",
        uri: "memory:Log",
        title: "Short Term Temporal Memory File",
        description:
          "Chronological event log with ISO 8601 timestamps. Append-only during sessions, consolidated into weekly journal during reflection.",
        mimeType: "text/markdown",
        annotations: {
          audience: ["assistant"],
          priority: 0.8,
        },
      },
      {
        name: "Working Memory",
        uri: "memory:Working Memory",
        title: "Short Term Memory File",
        description:
          "Temporary storage for discoveries and decisions. Organized into Knowledge Notes, Project Notes, and optional Episodic sections. Cleared after reflection.",
        mimeType: "text/markdown",
        annotations: {
          audience: ["assistant"],
          priority: 0.8,
        },
      },
      {
        name: "knowledge",
        uri: "memory:knowledge/",
        title: "Long Term Memory Directory",
        description:
          "Permanent technical knowledge notes. Term-based, dictionary-style entries covering programming languages, frameworks, design patterns, and concepts.",
        mimeType: "text/markdown",
        annotations: {
          audience: ["assistant", "user"],
          priority: 0.7,
        },
      },
      {
        name: "journal",
        uri: "memory:journal/",
        title: "Weekly Notes & Logs Directory",
        description:
          "Weekly notes in YYYY-wW.md format. Daily work logs organized by weekday, plus optional deeper notes.",
        mimeType: "text/markdown",
        annotations: {
          audience: ["assistant", "user"],
          priority: 0.6,
        },
      },
      {
        name: "private",
        uri: "memory:private/",
        title: "Private Long Term Memory Directory",
        description:
          "Sensitive or personal notes requiring explicit user approval before access. Not included in main knowledge graph.",
        mimeType: "text/markdown",
        annotations: {
          audience: ["user"],
          priority: 0.3,
        },
      },
    ],
  };
});

// Register all tools
registerGetNote(server, toolContext);
registerGetWeeklyNote(server, toolContext);
registerGetCurrentDatetime(server, toolContext);
registerLog(server, toolContext);
registerRemember(server, toolContext);
registerUpdateFrontmatter(server, toolContext);
registerGetGraphNeighborhood(server, toolContext);
registerGetNoteUsage(server, toolContext);
registerLoadPrivateMemory(server, toolContext);
registerReindex(server, toolContext);
registerCompleteReindex(server, toolContext);
registerReflect(server, toolContext);
registerCompleteReflect(server, toolContext);
registerSearch(server, toolContext);

// Start the server
async function main() {
  try {
    debugLog("[Server] Starting initialization...");

    // Initialize graph index and memory system
    debugLog("[Server] Initializing graph index...");
    await graphIndex.initialize();

    debugLog("[Server] Initializing memory system...");
    await memorySystem.initialize();

    // Initialize embedding manager
    debugLog("[Server] Initializing embedding manager...");
    embeddingManager = await EmbeddingManager.getInstance(vaultPath);

    // Initialize graph proximity manager
    debugLog("[Server] Initializing graph proximity manager...");
    graphProximityManager = await GraphProximityManager.getInstance(vaultPath, graphIndex);

    // Start warming up cache in background (non-blocking)
    // Search tool will wait for warmup to complete before first use
    debugLog("[Server] Starting cache warmup in background...");
    const warmupPromise = embeddingManager.warmupCache(vaultPath, graphIndex, fileOps)
      .then(() => {
        debugLog("[Server] Cache warmup completed");
      })
      .catch((error) => {
        debugLog(`[Server] Cache warmup failed: ${error}`);
      });

    // Store warmup promise in tool context so Search can await it
    (toolContext as any).warmupPromise = warmupPromise;

    // Register file change callback to invalidate caches
    graphIndex.onFileChange(async (filePath, event) => {
      if (event === 'change' || event === 'unlink') {
        try {
          // Convert absolute path to relative path for embedding cache
          const relativePath = path.relative(vaultPath, filePath);
          embeddingManager.invalidate(relativePath);
          await embeddingManager.saveCache();

          // Invalidate graph proximity cache for the changed note
          // (links might have changed, affecting proximity scores)
          const noteName = path.basename(filePath, '.md');
          graphProximityManager.invalidate(noteName);
          await graphProximityManager.saveCache();
        } catch (error) {
          debugLog(`[CacheManager] Error invalidating caches for ${filePath}: ${error}`);
        }
      }
    });

    debugLog("[Server] Connecting to transport...");
    const transport = new StdioServerTransport();

    // Add error handler for transport
    transport.onerror = (error) => {
      debugLog(`[Server] Transport error: ${error}`);
    };

    await server.server.connect(transport);

    debugLog("[Server] Obsidian Memory MCP Server running");
    console.error("[Server] Obsidian Memory MCP Server running");

    // Add error handler for the server
    server.server.onerror = (error) => {
      debugLog(`[Server] MCP Server error: ${error}`);
    };

    // Clean up on exit
    process.on("SIGINT", async () => {
      debugLog("[Server] Shutting down...");
      console.error("[Server] Shutting down...");

      // Save embedding cache to disk
      await embeddingManager.saveCache();

      await graphIndex.dispose();
      process.exit(0);
    });
  } catch (error) {
    debugLog(`[Server] Error during initialization: ${error}`);
    debugLog(`[Server] Stack trace: ${(error as Error).stack}`);
    throw error;
  }
}

main().catch((error) => {
  debugLog(`[Server] Fatal error: ${error}`);
  debugLog(`[Server] Stack trace: ${(error as Error).stack}`);
  console.error("[Server] Fatal error:", error);
  process.exit(1);
});
