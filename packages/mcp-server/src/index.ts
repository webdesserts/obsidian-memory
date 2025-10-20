#!/usr/bin/env node

import { basename } from "node:path";
import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import {
  CallToolRequestSchema,
  ListToolsRequestSchema,
  ListResourcesRequestSchema,
  ReadResourceRequestSchema,
  ListRootsRequestSchema,
  ListResourceTemplatesRequestSchema,
} from "@modelcontextprotocol/sdk/types.js";
import { FileOperations } from "./file-operations.js";
import { GraphIndex } from "./graph/graph-index.js";
import { MemorySystem } from "./memory/memory-system.js";
import { ConsolidationManager } from "./memory/consolidation.js";
import { resolveNotePath } from "@obsidian-memory/utils";
import { allTools } from "./tools/index.js";
import { ToolContext } from "./types.js";
import { readNoteResource } from "./resource-utils.js";

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
      roots: {
        listChanged: false,
      },
    },
  }
);

// List roots
server.setRequestHandler(ListRootsRequestSchema, async () => {
  return {
    roots: [
      {
        uri: `file://${vaultPath}`,
        name: vaultName,
      },
    ],
  };
});

// List available tools
server.setRequestHandler(ListToolsRequestSchema, async () => {
  return {
    tools: allTools.map((tool) => tool.definition),
  };
});

// List resource templates
server.setRequestHandler(ListResourceTemplatesRequestSchema, async () => {
  // No resource templates - use GetNote tool for note discovery
  return {
    resourceTemplates: [],
  };
});

// List available resources
server.setRequestHandler(ListResourcesRequestSchema, async () => {
  return {
    resources: [
      {
        uri: "memory:Working Memory",
        name: "Working Memory",
        description:
          "Scratchpad for temporary notes and research. Update freely. Notes here may periodically be moved over to permanent notes or removed when appropriate.",
        mimeType: "text/markdown",
        annotations: {
          audience: ["assistant"],
          priority: 1.0,
        },
      },
      {
        uri: "memory:Index",
        name: "Long-term Memory Index",
        description:
          "List of commonly accessed notes and journal entries. Refreshed in longer intervals. Used as an entry point for exploring the knowledge graph",
        mimeType: "text/markdown",
        annotations: {
          audience: ["assistant"],
          priority: 9,
        },
      },
      {
        uri: "memory:private/Working Memory",
        name: "Private Working Memory",
        description:
          "Scratchpad for temporary notes and research that may contain sensitive or personal information. Always ask for explicit user consent before reading this resource.",
        mimeType: "text/markdown",
        annotations: {
          audience: ["assistant"],
          priority: 0.5,
        },
      },
      {
        uri: "memory:private/Index",
        name: "Private Long-term Memory Index",
        description:
          "List of commonly accessed notes and journal entries that may contain sensitive or personal information. Always ask for explicit user consent before reading this resource.",
        mimeType: "text/markdown",
        annotations: {
          audience: ["assistant"],
          priority: 0.5,
        },
      },
    ],
  };
});

// Handle resource reads (static resources only)
server.setRequestHandler(ReadResourceRequestSchema, async (request) => {
  const uri = request.params.uri;

  try {
    // Parse memory: URI
    if (!uri.startsWith("memory:")) {
      throw new Error(`Unsupported URI scheme: ${uri}`);
    }

    // Extract path from URI
    const notePath = uri.replace("memory:", "");

    // Only support static resources (Index, Working Memory, private variants)
    const staticResources = [
      "Index",
      "Working Memory",
      "private/Index",
      "private/Working Memory",
    ];

    if (!staticResources.includes(notePath)) {
      throw new Error(
        `Only static resources are supported. Use GetNote tool for other notes: ${notePath}`
      );
    }

    // Read the static resource
    const result = await readNoteResource({
      noteRef: uri,
      context: toolContext,
    });

    // If the result is an error (note doesn't exist), return helpful message
    if (result.isError && result.content[0].type === "text") {
      return {
        contents: [
          {
            uri,
            mimeType: "text/markdown",
            text: `# ${notePath}\n\n*This file does not exist yet. Create it to start using this memory space.*`,
          },
        ],
      };
    }

    // Extract the resource content from the tool response
    if (result.content[0].type === "resource") {
      return {
        contents: [
          {
            uri: result.content[0].resource.uri,
            mimeType: result.content[0].resource.mimeType,
            text: result.content[0].resource.text || "",
          },
        ],
      };
    }

    throw new Error(`Unexpected response format from readNoteResource`);
  } catch (error) {
    const errorMessage = error instanceof Error ? error.message : String(error);
    throw new Error(`Failed to read resource ${uri}: ${errorMessage}`);
  }
});

// Handle tool calls
server.setRequestHandler(CallToolRequestSchema, async (request) => {
  const { name, arguments: args = {} } = request.params;

  try {
    const tool = allTools.find((tool) => tool.definition.name === name);

    if (!tool) throw new Error(`Tool not found: ${name}`);

    return tool.handler(args, toolContext);
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
