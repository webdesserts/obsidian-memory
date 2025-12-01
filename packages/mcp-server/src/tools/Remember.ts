import fs from "fs/promises";
import path from "path";
import type { McpServer } from "../server.js";
import type { ToolContext } from "../types.js";
import { getCurrentWeeklyNotePath } from "../week-utils.js";
import { discoverProjects } from "../projects/discovery.js";
import { generateDiscoveryStatusMessage } from "../projects/messages.js";

/**
 * Remember Tool
 *
 * Load all session context files in a single call.
 * Returns Log.md, Working Memory.md, Index.md, and current weekly note.
 */
export function registerRemember(server: McpServer, context: ToolContext) {
  server.registerTool(
    "Remember",
    {
      title: "Remember Session Context",
      description:
        "Load all session context files in a single call. " +
        "Returns Log.md, Working Memory.md, current weekly note, and discovered project notes. " +
        "Automatically discovers projects based on git remotes and directory names. " +
        "Use this at the start of every session to get complete context about recent work, " +
        "current focus, this week's activity, and project context.",
      inputSchema: {},
      annotations: {
        readOnlyHint: true,
        openWorldHint: false,
      },
    },
    async () => {
      const { vaultPath, graphIndex } = context;

      // Define paths to all context files
      const logPath = path.join(vaultPath, "Log.md");
      const workingMemoryPath = path.join(vaultPath, "Working Memory.md");

      // Get weekly note path
      const weeklyNoteUri = getCurrentWeeklyNotePath();
      const url = new URL(weeklyNoteUri);
      const weeklyNotePath = url.pathname;

      // Discover projects for current working directory
      const cwd = process.cwd();
      const discoveryResult = discoverProjects(cwd, graphIndex, vaultPath);

      // Read all context files in parallel
      const [logContent, workingMemoryContent, weeklyNoteContent] =
        await Promise.all([
          fs.readFile(logPath, "utf-8").catch(() => ""),
          fs.readFile(workingMemoryPath, "utf-8").catch(() => ""),
          fs.readFile(weeklyNotePath, "utf-8").catch(() => ""),
        ]);

      // Read strict match project notes
      const projectContents = await Promise.all(
        discoveryResult.strictMatches.map(async (match) => ({
          match,
          content: await fs
            .readFile(match.metadata.filePath, "utf-8")
            .catch(() => ""),
        }))
      );

      // Build content blocks array - one resource per file
      const contentBlocks = [];

      if (logContent) {
        contentBlocks.push({
          type: "resource" as const,
          resource: {
            uri: `file://${logPath}`,
            mimeType: "text/markdown",
            text: logContent,
          },
        });
      }

      if (workingMemoryContent) {
        contentBlocks.push({
          type: "resource" as const,
          resource: {
            uri: `file://${workingMemoryPath}`,
            mimeType: "text/markdown",
            text: workingMemoryContent,
          },
        });
      }

      if (weeklyNoteContent) {
        contentBlocks.push({
          type: "resource" as const,
          resource: {
            uri: weeklyNoteUri,
            mimeType: "text/markdown",
            text: weeklyNoteContent,
          },
        });
      }

      // Add strictly matched project notes
      for (const { match, content } of projectContents) {
        if (content) {
          contentBlocks.push({
            type: "resource" as const,
            resource: {
              uri: `file://${match.metadata.filePath}`,
              mimeType: "text/markdown",
              text: content,
            },
          });
        }
      }

      // Generate project discovery status message
      const projectStatus = generateDiscoveryStatusMessage(discoveryResult, cwd);

      // Add project status as text content if not empty
      if (projectStatus) {
        contentBlocks.push({
          type: "text" as const,
          text: projectStatus,
        });
      }

      return {
        content: contentBlocks,
        structuredContent: {
          filesLoaded: contentBlocks.filter((b) => b.type === "resource").length,
          projectsFound: discoveryResult.strictMatches.length,
          projectDisconnects: discoveryResult.looseMatches.length,
          projectSuggestions: discoveryResult.suggestions.length,
        },
      };
    }
  );
}
