#!/usr/bin/env node

import { Server } from "@modelcontextprotocol/sdk/server/index.js";
import { StdioServerTransport } from "@modelcontextprotocol/sdk/server/stdio.js";
import {
  CallToolRequestSchema,
  ListToolsRequestSchema,
  ListResourcesRequestSchema,
  ReadResourceRequestSchema,
} from "@modelcontextprotocol/sdk/types.js";
import { FileOperations } from "./tools/file-operations.js";
import { GraphIndex } from "./graph/graph-index.js";
import { MemorySystem } from "./memory/memory-system.js";
import { ConsolidationManager } from "./memory/consolidation.js";
import {
  normalizeNoteReference,
  extractNoteName,
  generateSearchPaths,
  resolveNotePath,
} from "@obsidian-memory/utils";

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
    tools: [
      {
        name: "read_note",
        description: "Read the content of a note from the vault",
        inputSchema: {
          type: "object",
          properties: {
            note: {
              type: "string",
              description:
                "Note name or path. Supports: 'Note Name', 'Note Name.md', 'knowledge/Note Name', 'memory://knowledge/Note Name'",
            },
          },
          required: ["note"],
        },
      },
      {
        name: "get_frontmatter",
        description: "Get the frontmatter metadata from a note",
        inputSchema: {
          type: "object",
          properties: {
            path: {
              type: "string",
              description: "Path to the note relative to vault root",
            },
          },
          required: ["path"],
        },
      },
      {
        name: "update_frontmatter",
        description: "Update frontmatter metadata in a note",
        inputSchema: {
          type: "object",
          properties: {
            path: {
              type: "string",
              description: "Path to the note relative to vault root",
            },
            updates: {
              type: "object",
              description: "Frontmatter fields to update",
            },
          },
          required: ["path", "updates"],
        },
      },
      {
        name: "get_backlinks",
        description: "Find all notes that link to a given note",
        inputSchema: {
          type: "object",
          properties: {
            noteName: {
              type: "string",
              description: "The note name (without .md extension)",
            },
            includePrivate: {
              type: "boolean",
              description: "Include links from private folder (default: false)",
            },
          },
          required: ["noteName"],
        },
      },
      {
        name: "get_graph_neighborhood",
        description:
          "Explore notes connected to a note via wiki links (primary discovery tool)",
        inputSchema: {
          type: "object",
          properties: {
            noteName: {
              type: "string",
              description: "The note name to explore from",
            },
            depth: {
              type: "number",
              description:
                "How many hops to explore (1-3 recommended, default: 2)",
            },
            includePrivate: {
              type: "boolean",
              description: "Include private folder notes (default: false)",
            },
          },
          required: ["noteName"],
        },
      },
      {
        name: "get_note_usage",
        description: "Get usage statistics for notes (for consolidation)",
        inputSchema: {
          type: "object",
          properties: {
            notes: {
              type: "array",
              items: { type: "string" },
              description: "List of note names to get statistics for",
            },
            period: {
              type: "string",
              enum: ["24h", "7d", "30d", "all"],
              description: "Time period for statistics (default: all)",
            },
          },
          required: ["notes"],
        },
      },
      {
        name: "load_private_memory",
        description:
          "Load private memory indexes (requires explicit user consent)",
        inputSchema: {
          type: "object",
          properties: {
            reason: {
              type: "string",
              description: "Reason for loading private memory",
            },
          },
          required: ["reason"],
        },
      },
      {
        name: "consolidate_memory",
        description:
          "Trigger memory consolidation (consolidate WorkingMemory.md into Index.md)",
        inputSchema: {
          type: "object",
          properties: {
            includePrivate: {
              type: "boolean",
              description:
                "Include private notes in consolidation (default: false)",
            },
          },
        },
      },
      {
        name: "complete_consolidation",
        description:
          "Mark consolidation as complete (deletes WorkingMemory.md, releases lock)",
        inputSchema: {
          type: "object",
          properties: {},
        },
      },
    ],
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
    switch (name) {
      case "read_note": {
        const { note } = args as { note: string };

        // Normalize the note reference
        const notePath = normalizeNoteReference(note);
        const noteNameOnly = extractNoteName(notePath);

        // Determine final path using smart lookup
        let finalPath: string = notePath; // Default to provided path

        // If path includes a folder, use it directly
        if (notePath.includes("/")) {
          finalPath = notePath;
        } else {
          // Smart lookup: try common locations in priority order
          const searchPaths = generateSearchPaths(noteNameOnly, false);

          let found = false;
          // Try each path until we find one that exists
          for (const searchPath of searchPaths) {
            try {
              await fileOps.readNote(searchPath);
              finalPath = searchPath;
              found = true;
              break;
            } catch {
              // Continue to next path
            }
          }

          // Fall back to graph index if not found in standard paths
          if (!found) {
            const indexPath = graphIndex.getNotePath(noteNameOnly);
            if (indexPath) {
              finalPath = indexPath;
            }
          }
        }

        // Read the note
        const result = await fileOps.readNote(finalPath);

        // Log note access for usage statistics
        memorySystem.logAccess(noteNameOnly, "read_note");

        // Build metadata
        const metadata = {
          noteName: noteNameOnly,
          memoryUri: `memory://${finalPath}`,
          filePath: `${vaultPath}/${finalPath}.md`,
        };

        // Build response with metadata first, then content
        let response = `\`\`\`json\n${JSON.stringify(
          metadata,
          null,
          2
        )}\n\`\`\`\n\n`;

        if (result.frontmatter) {
          response += `---\nFrontmatter:\n${JSON.stringify(
            result.frontmatter,
            null,
            2
          )}\n---\n\n`;
        }

        response += result.content;

        return {
          content: [{ type: "text", text: response }],
        };
      }

      case "get_frontmatter": {
        const { path } = args as { path: string };
        const frontmatter = await fileOps.getFrontmatter(path);

        return {
          content: [
            {
              type: "text",
              text: frontmatter
                ? JSON.stringify(frontmatter, null, 2)
                : "No frontmatter found",
            },
          ],
        };
      }

      case "update_frontmatter": {
        const { path, updates } = args as {
          path: string;
          updates: Record<string, any>;
        };

        await fileOps.updateFrontmatter(path, updates);

        return {
          content: [{ type: "text", text: `Frontmatter updated: ${path}` }],
        };
      }

      case "get_backlinks": {
        const { noteName, includePrivate = false } = args as {
          noteName: string;
          includePrivate?: boolean;
        };

        // Resolve note name to actual path (handles duplicates)
        const resolvedPath = resolveNoteNameToPath(noteName, includePrivate);
        if (!resolvedPath) {
          return {
            content: [
              { type: "text", text: `Note not found in graph: ${noteName}` },
            ],
          };
        }

        // Use the note name from the resolved path
        const resolvedNoteName = extractNoteName(resolvedPath);
        const backlinks = graphIndex.getBacklinks(resolvedNoteName, includePrivate);

        if (backlinks.length === 0) {
          return {
            content: [
              {
                type: "text",
                text: `No backlinks found for: ${resolvedNoteName} (${resolvedPath})`,
              },
            ],
          };
        }

        // Build ResourceLinks for each backlink
        const resourceLinks = backlinks.map((note) => {
          const notePath = graphIndex.getNotePath(note) || note;
          return {
            type: "resource" as const,
            resource: {
              uri: `memory://${notePath}`,
              name: note,
              mimeType: "text/markdown",
              description: `Links to [[${noteName}]]`,
            },
          };
        });

        // Also include a text summary
        const summary = {
          type: "text" as const,
          text: `Found ${backlinks.length} backlink${
            backlinks.length === 1 ? "" : "s"
          } to "${noteName}"`,
        };

        return {
          content: [summary, ...resourceLinks],
        };
      }

      case "get_graph_neighborhood": {
        const {
          noteName,
          depth = 2,
          includePrivate = false,
        } = args as {
          noteName: string;
          depth?: number;
          includePrivate?: boolean;
        };

        // Resolve note name to actual path (handles duplicates)
        const resolvedPath = resolveNoteNameToPath(noteName, includePrivate);
        if (!resolvedPath) {
          return {
            content: [
              { type: "text", text: `Note not found in graph: ${noteName}` },
            ],
          };
        }

        // Use the note name from the resolved path
        const resolvedNoteName = extractNoteName(resolvedPath);
        const neighborhood = graphIndex.getNeighborhood(
          resolvedNoteName,
          depth,
          includePrivate
        );

        if (neighborhood.size === 0) {
          return {
            content: [
              {
                type: "text",
                text: `No connected notes found for: ${resolvedNoteName} (${resolvedPath})`,
              },
            ],
          };
        }

        // Build text summary
        let summary = `Graph neighborhood for "${resolvedNoteName}" at ${resolvedPath} (depth: ${depth}):\n\n`;

        // Build ResourceLinks grouped by distance
        const resourceLinks: any[] = [];

        for (let d = 1; d <= depth; d++) {
          const notesAtDistance = Array.from(neighborhood.entries()).filter(
            ([_, info]) => info.distance === d
          );

          if (notesAtDistance.length > 0) {
            summary += `Distance ${d}: ${notesAtDistance.length} note${
              notesAtDistance.length === 1 ? "" : "s"
            }\n`;

            for (const [note, info] of notesAtDistance) {
              const notePath = graphIndex.getNotePath(note) || note;

              // Build description with link information
              let description = `${info.linkType} (distance ${d})`;
              if (info.directLinks.length > 0) {
                description += ` - Links to: ${info.directLinks.join(", ")}`;
              }
              if (info.backlinks.length > 0) {
                description += ` - Linked from: ${info.backlinks.join(", ")}`;
              }

              resourceLinks.push({
                type: "resource" as const,
                resource: {
                  uri: `memory://${notePath}`,
                  name: note,
                  mimeType: "text/markdown",
                  description,
                },
              });
            }
          }
        }

        return {
          content: [{ type: "text" as const, text: summary }, ...resourceLinks],
        };
      }

      case "get_note_usage": {
        const { notes, period = "all" } = args as {
          notes: string[];
          period?: "24h" | "7d" | "30d" | "all";
        };

        const stats = await memorySystem.getNoteUsage(notes, period);

        // Add backlink counts from graph index
        for (const note of notes) {
          const backlinks = graphIndex.getBacklinks(note, false);
          stats[note].backlinks = backlinks.length;
        }

        return {
          content: [
            {
              type: "text",
              text: JSON.stringify(stats, null, 2),
            },
          ],
        };
      }

      case "load_private_memory": {
        const { reason } = args as { reason: string };

        console.error(`[MemorySystem] Loading private memory: ${reason}`);

        const { longTermIndex, workingMemory } =
          await memorySystem.loadPrivateMemory();

        let response = `# Private Memory Loaded\n\nReason: ${reason}\n\n`;

        if (longTermIndex) {
          response += `## Private Index.md\n\n${longTermIndex}\n\n`;
        } else {
          response += `## Private Index.md\n\nNo private Index.md found\n\n`;
        }

        if (workingMemory) {
          response += `## Private WorkingMemory.md\n\n${workingMemory}\n\n`;
        } else {
          response += `## Private WorkingMemory.md\n\nNo private WorkingMemory.md found\n\n`;
        }

        return {
          content: [{ type: "text", text: response }],
        };
      }

      case "consolidate_memory": {
        const { includePrivate = false } = args as { includePrivate?: boolean };

        console.error(
          `[Consolidation] Triggering consolidation (includePrivate: ${includePrivate})`
        );

        const prompt = await consolidationManager.triggerConsolidation(
          includePrivate
        );

        return {
          content: [{ type: "text", text: prompt }],
        };
      }

      case "complete_consolidation": {
        console.error("[Consolidation] Completing consolidation");

        await consolidationManager.completeConsolidation();

        return {
          content: [
            {
              type: "text",
              text: "Consolidation complete! WorkingMemory.md deleted, Index.md reloaded.",
            },
          ],
        };
      }

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
