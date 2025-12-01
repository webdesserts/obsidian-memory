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
import { EmbeddingManager } from "./embeddings/manager.js";
import { GraphProximityManager } from "./embeddings/graph-manager.js";
import { resolveNotePath } from "@webdesserts/obsidian-memory-utils";
import { ToolContext } from "./types.js";
import path from "path";
import { logger } from "./utils/logger.js";

// Global error handlers - log all uncaught errors to debug.log
process.on("uncaughtException", (error) => {
  logger.fatal({ group: "Process", err: error }, "Uncaught exception");
  process.exit(1);
});

process.on("unhandledRejection", (reason, promise) => {
  logger.fatal({ group: "Process", promise, reason }, "Unhandled rejection");
  process.exit(1);
});

// Tool registrations
import { registerGetNote } from "./tools/GetNote.js";
import { registerGetWeeklyNote } from "./tools/GetWeeklyNote.js";
import { registerGetCurrentDatetime } from "./tools/GetCurrentDatetime.js";
import { registerLog } from "./tools/Log.js";
import { registerRemember } from "./tools/Remember.js";
import { registerUpdateFrontmatter } from "./tools/UpdateFrontmatter.js";
import { registerLoadPrivateMemory } from "./tools/LoadPrivateMemory.js";
import { registerReflect } from "./tools/Reflect.js";
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

logger.info({ group: "Server" }, "Starting Obsidian Memory MCP Server");
logger.info({ group: "Server", vaultPath, vaultName }, "Vault configured");

// Initialize file operations, graph index, memory system, and embedding manager
const fileOps = new FileOperations({ vaultPath });
const graphIndex = new GraphIndex(vaultPath);
const memorySystem = new MemorySystem(vaultPath, fileOps);

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
registerLoadPrivateMemory(server, toolContext);
registerReflect(server, toolContext);
registerSearch(server, toolContext);

// Start the server
async function main() {
  try {
    logger.info({ group: "Server" }, "Starting initialization");

    // Initialize graph index and memory system
    logger.info({ group: "Server" }, "Initializing graph index");
    await graphIndex.initialize();

    logger.info({ group: "Server" }, "Initializing memory system");
    await memorySystem.initialize();

    // Initialize embedding manager
    logger.info({ group: "Server" }, "Initializing embedding manager");
    embeddingManager = await EmbeddingManager.getInstance(vaultPath);

    // Initialize graph proximity manager
    logger.info({ group: "Server" }, "Initializing graph proximity manager");
    graphProximityManager = await GraphProximityManager.getInstance(vaultPath, graphIndex);

    // Start warming up cache in background (non-blocking)
    // Search tool will wait for warmup to complete before first use
    logger.info({ group: "Server" }, "Starting cache warmup in background");
    const warmupPromise = embeddingManager.warmupCache(vaultPath, graphIndex, fileOps)
      .then(() => {
        logger.info({ group: "Server" }, "Cache warmup completed");
      })
      .catch((error) => {
        logger.error({ group: "Server", err: error }, "Cache warmup failed");
      });

    // Store warmup promise in tool context so Search can await it
    (toolContext as any).warmupPromise = warmupPromise;

    // Register file change callback to invalidate caches
    graphIndex.onFileChange(async (filePath, event) => {
      if (event === 'change' || event === 'unlink') {
        try {
          const relativePath = path.relative(vaultPath, filePath);

          // Invalidate embedding cache
          embeddingManager.invalidate(relativePath);
          await embeddingManager.saveCache();

          // Invalidate graph proximity cache for the changed note
          // (links might have changed, affecting proximity scores)
          const noteName = path.basename(filePath, '.md');
          graphProximityManager.invalidate(noteName);
          await graphProximityManager.saveCache();
        } catch (error) {
          logger.error({ group: "CacheManager", filePath, err: error }, "Error invalidating caches");
        }
      }
    });

    logger.info({ group: "Server" }, "Connecting to transport");
    const transport = new StdioServerTransport();

    // Add error handler for transport
    transport.onerror = (error) => {
      logger.error({ group: "Server", err: error }, "Transport error");
    };

    await server.server.connect(transport);

    logger.info({ group: "Server" }, "Obsidian Memory MCP Server running");

    // Add error handler for the server
    server.server.onerror = (error) => {
      logger.error({ group: "Server", err: error }, "MCP Server error");
    };

    // Clean up on exit
    process.on("SIGINT", async () => {
      logger.info({ group: "Server" }, "Shutting down");

      // Save embedding cache to disk
      await embeddingManager.saveCache();

      await graphIndex.dispose();
      process.exit(0);
    });
  } catch (error) {
    logger.error({ group: "Server", err: error }, "Error during initialization");
    throw error;
  }
}

main().catch((error) => {
  logger.fatal({ group: "Server", err: error }, "Fatal error");
  process.exit(1);
});
