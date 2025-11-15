import fs from "fs/promises";
import path from "path";
import { z } from "zod";
import type { McpServer } from "../server.js";
import type { ToolContext } from "../types.js";
import { getCurrentWeeklyNotePath } from "../week-utils.js";

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
        "Returns Log.md, Working Memory.md, Index.md, and current weekly note. " +
        "Use this at the start of every session to get complete context about recent work, " +
        "current focus, knowledge index, and this week's activity.",
      inputSchema: {},
      annotations: {
        readOnlyHint: true,
        openWorldHint: false,
      },
    },
    async () => {
      const { vaultPath } = context;

      // Define paths to all context files
      const logPath = path.join(vaultPath, "Log.md");
      const workingMemoryPath = path.join(vaultPath, "Working Memory.md");
      const indexPath = path.join(vaultPath, "Index.md");

      // Get weekly note path
      const weeklyNoteUri = getCurrentWeeklyNotePath();
      const url = new URL(weeklyNoteUri);
      const weeklyNotePath = url.pathname;

      // Read all files in parallel
      const [logContent, workingMemoryContent, indexContent, weeklyNoteContent] =
        await Promise.all([
          fs.readFile(logPath, "utf-8").catch(() => ""),
          fs.readFile(workingMemoryPath, "utf-8").catch(() => ""),
          fs.readFile(indexPath, "utf-8").catch(() => ""),
          fs.readFile(weeklyNotePath, "utf-8").catch(() => ""),
        ]);

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

      if (indexContent) {
        contentBlocks.push({
          type: "resource" as const,
          resource: {
            uri: `file://${indexPath}`,
            mimeType: "text/markdown",
            text: indexContent,
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

      return {
        content: contentBlocks,
        structuredContent: {
          filesLoaded: contentBlocks.length,
        },
      };
    }
  );
}
