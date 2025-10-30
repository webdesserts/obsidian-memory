import { z } from "zod";
import type { McpServer } from "../server.js";
import type { ToolContext } from "../types.js";
import {
  getCurrentWeeklyNotePath,
  getCurrentDayOfWeek,
  getISOWeek,
  getISOWeekYear,
} from "../week-utils.js";

/**
 * GetWeeklyNote Tool
 *
 * Get the URI for the current week's journal note.
 */
export function registerGetWeeklyNote(server: McpServer, context: ToolContext) {
  server.registerTool(
    "GetWeeklyNote",
    {
      title: "Get Weekly Note",
      description:
        "Get the URI for the current week's journal note. Returns a resource link that can be read to access the note content.",
      inputSchema: {},
      annotations: {
        readOnlyHint: true,
        openWorldHint: false,
      },
    },
    async () => {
      // Get current weekly note info
      const weeklyNoteUri = getCurrentWeeklyNotePath();
      const currentDay = getCurrentDayOfWeek();
      const now = new Date();
      const week = getISOWeek(now);
      const year = getISOWeekYear(now);

      // Extract path from URI for the name
      const url = new URL(weeklyNoteUri);
      const path = url.pathname;

      // Return ResourceLink to the weekly note
      return {
        content: [
          {
            type: "resource_link",
            uri: weeklyNoteUri,
            name: `${year} Week ${week}`,
            mimeType: "text/markdown",
            description: `Current weekly journal (${currentDay})`,
          },
        ],
        structuredContent: {
          currentDay,
          week,
          year,
          path,
        },
      };
    }
  );
}
