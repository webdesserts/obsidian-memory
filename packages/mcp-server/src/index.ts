#!/usr/bin/env node

import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import {
  CallToolRequestSchema,
  ListToolsRequestSchema,
  ListResourcesRequestSchema,
  ReadResourceRequestSchema,
} from "@modelcontextprotocol/sdk/types.js";
import { FileOperations } from "./file-operations.js";
import { GraphIndex } from "./graph/graph-index.js";
import { MemorySystem } from "./memory/memory-system.js";
import { ConsolidationManager } from "./memory/consolidation.js";
import { resolveNotePath } from "@obsidian-memory/utils";
import { allTools, ToolContext } from "./tools/index.js";
import { readNoteTool } from "./tools/read-note.js";
import { getFrontmatterTool } from "./tools/get-frontmatter.js";
import { updateFrontmatterTool } from "./tools/update-frontmatter.js";
import { getBacklinksTool } from "./tools/get-backlinks.js";
import { getGraphNeighborhoodTool } from "./tools/get-graph-neighborhood.js";
import { getNoteUsageTool } from "./tools/get-note-usage.js";
import { loadPrivateMemoryTool } from "./tools/load-private-memory.js";
import { consolidateMemoryTool } from "./tools/consolidate-memory.js";
import { completeConsolidationTool } from "./tools/complete-consolidation.js";

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

console.error(`[Server] Starting Obsidian Memory MCP Server`);
console.error(`[Server] Vault path: ${vaultPath}`);

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
  fileOps,
  graphIndex,
  memorySystem,
  consolidationManager,
  resolveNoteNameToPath,
} satisfies ToolContext;

// Create server instance
const server = new Server(
  {
    name: "obsidian-memory-mcp",
    version: "0.1.0",
  },
  {
    capabilities: {
      tools: {},
      resources: {
        subscribe: true,
      },
    },
  }
);

// List available tools
server.setRequestHandler(ListToolsRequestSchema, async () => {
  return {
    tools: allTools.map((tool) => tool.definition),
  };
});

// List available resources
server.setRequestHandler(ListResourcesRequestSchema, async () => {
  return {
    resources: [
      {
        uri: "memory://Index",
        name: "Long-term Memory Index",
        description:
          "Public long-term memory - stable entry points organized by domain",
        mimeType: "text/markdown",
      },
      {
        uri: "memory://WorkingMemory",
        name: "Working Memory",
        description:
          "Public short-term memory - notes and discoveries from recent sessions",
        mimeType: "text/markdown",
      },
      {
        uri: "memory://private/Index",
        name: "Private Long-term Memory Index",
        description:
          "Personal and sensitive long-term memory. Contains private notes and information. Always ask for explicit user consent before reading this resource.",
        mimeType: "text/markdown",
      },
      {
        uri: "memory://private/WorkingMemory",
        name: "Private Working Memory",
        description:
          "Personal and sensitive short-term memory. Contains private notes from recent sessions. Always ask for explicit user consent before reading this resource.",
        mimeType: "text/markdown",
      },
    ],
  };
});

// Handle resource reads
server.setRequestHandler(ReadResourceRequestSchema, async (request) => {
  const uri = request.params.uri;

  try {
    // Parse memory:// URI
    if (!uri.startsWith("memory://")) {
      throw new Error(`Unsupported URI scheme: ${uri}`);
    }

    const resourcePath = uri.slice(9); // Remove "memory://"

    let content: string;
    let exists = true;

    switch (resourcePath) {
      case "Index":
        content = memorySystem.getIndex() || "";
        exists = !!memorySystem.getIndex();
        break;

      case "WorkingMemory":
        content = memorySystem.getWorkingMemory() || "";
        exists = !!memorySystem.getWorkingMemory();
        break;

      case "private/Index": {
        const { longTermIndex } = await memorySystem.loadPrivateMemory();
        content = longTermIndex || "";
        exists = !!longTermIndex;
        break;
      }

      case "private/WorkingMemory": {
        const { workingMemory } = await memorySystem.loadPrivateMemory();
        content = workingMemory || "";
        exists = !!workingMemory;
        break;
      }

      default:
        throw new Error(`Unknown resource: ${uri}`);
    }

    if (!exists) {
      content = `# ${resourcePath}\n\n*This file does not exist yet. Create it to start using this memory space.*`;
    }

    return {
      contents: [
        {
          uri,
          mimeType: "text/markdown",
          text: content,
        },
      ],
    };
  } catch (error) {
    const errorMessage = error instanceof Error ? error.message : String(error);
    throw new Error(`Failed to read resource ${uri}: ${errorMessage}`);
  }
});

// Handle tool calls
server.setRequestHandler(CallToolRequestSchema, async (request) => {
  const { name, arguments: args } = request.params;

  try {
    // Dispatch to tool handlers (using switch for type narrowing)
    switch (name) {
      case readNoteTool.name:
        return await readNoteTool.handler(args, toolContext);

      case getFrontmatterTool.name:
        return await getFrontmatterTool.handler(args, toolContext);

      case updateFrontmatterTool.name:
        return await updateFrontmatterTool.handler(args, toolContext);

      case getBacklinksTool.name:
        return await getBacklinksTool.handler(args, toolContext);

      case getGraphNeighborhoodTool.name:
        return await getGraphNeighborhoodTool.handler(args, toolContext);

      case getNoteUsageTool.name:
        return await getNoteUsageTool.handler(args, toolContext);

      case loadPrivateMemoryTool.name:
        return await loadPrivateMemoryTool.handler(args, toolContext);

      case consolidateMemoryTool.name:
        return await consolidateMemoryTool.handler(args, toolContext);

      case completeConsolidationTool.name:
        return await completeConsolidationTool.handler(args, toolContext);

      default:
        throw new Error(`Unknown tool: ${name}`);
    }
  } catch (error) {
    const errorMessage = error instanceof Error ? error.message : String(error);
    return {
      content: [{ type: "text", text: `Error: ${errorMessage}` }],
      isError: true,
    };
  }
});

// Start the server
async function main() {
  // Initialize graph index and memory system
  await graphIndex.initialize();
  await memorySystem.initialize();

  const transport = new StdioServerTransport();
  await server.connect(transport);
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
