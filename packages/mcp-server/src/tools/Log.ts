import fs from "fs/promises";
import path from "path";
import { z } from "zod";
import type { McpServer } from "../server.js";
import type { ToolContext } from "../types.js";

/**
 * Format date as ISO 8601 datetime (YYYY-MM-DDTHH:MM)
 */
function formatISO8601DateTime(date: Date): string {
  const year = date.getFullYear();
  const month = String(date.getMonth() + 1).padStart(2, "0");
  const day = String(date.getDate()).padStart(2, "0");
  const hours = String(date.getHours()).padStart(2, "0");
  const minutes = String(date.getMinutes()).padStart(2, "0");

  return `${year}-${month}-${day}T${hours}:${minutes}`;
}

/**
 * Log Tool
 *
 * Append a timestamped entry to Log.md for temporal memory tracking.
 */
export function registerLog(server: McpServer, context: ToolContext) {
  server.registerTool(
    "Log",
    {
      title: "Log Timeline Entry",
      description:
        "Append a timestamped entry to Log.md for temporal memory tracking. " +
        "Records chronological session activity - what happened when. " +
        "The tool automatically adds ISO 8601 timestamps. " +
        "Include work ticket tags (e.g., [LOR-4883]) when logging work on specific items. " +
        "Use this for tracking session milestones, completed tasks, and temporal sequence of events.",
      inputSchema: {
        content: z
          .string()
          .describe(
            "Timeline entry content as bullet points. Tool adds timestamp automatically. " +
            "Tag work items with [TICKET-123] format when relevant."
          ),
      },
      annotations: {
        readOnlyHint: false,
        destructiveHint: false,
        openWorldHint: false,
      },
    },
    async ({ content }) => {
      const { vaultPath } = context;

      // Generate timestamp
      const now = new Date();
      const timestamp = formatISO8601DateTime(now);

      // Format entry with timestamp header
      const entry = `## ${timestamp}\n${content}\n`;

      // Append to Log.md
      const logPath = path.join(vaultPath, "Log.md");
      await fs.appendFile(logPath, entry + "\n");

      return {
        content: [
          {
            type: "text",
            text: `Logged at ${timestamp}`,
          },
        ],
        structuredContent: {
          timestamp,
        },
      };
    }
  );
}
