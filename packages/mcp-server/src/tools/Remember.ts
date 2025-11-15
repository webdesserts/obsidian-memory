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

      // Build combined output
      let output = "# Session Context\n\n";
      output += "Complete context loaded from memory system.\n\n";
      output += "---\n\n";

      // Log.md
      if (logContent) {
        output += "## Log.md - Recent Activity\n\n";
        output += logContent;
        output += "\n\n---\n\n";
      }

      // Working Memory.md
      if (workingMemoryContent) {
        output += "## Working Memory.md - Current Focus\n\n";
        output += workingMemoryContent;
        output += "\n\n---\n\n";
      }

      // Index.md
      if (indexContent) {
        output += "## Index.md - Knowledge Entry Points\n\n";
        output += indexContent;
        output += "\n\n---\n\n";
      }

      // Weekly Note
      if (weeklyNoteContent) {
        output += "## Weekly Note - This Week's Activity\n\n";
        output += weeklyNoteContent;
        output += "\n\n";
      }

      return {
        content: [
          {
            type: "text",
            text: output,
          },
        ],
        structuredContent: {
          filesLoaded: [
            logContent ? "Log.md" : null,
            workingMemoryContent ? "Working Memory.md" : null,
            indexContent ? "Index.md" : null,
            weeklyNoteContent ? "Weekly Note" : null,
          ].filter(Boolean),
        },
      };
    }
  );
}
