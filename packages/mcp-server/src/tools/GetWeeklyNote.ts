import { z } from "zod";
import { ToolContext, MCPTool } from "../types.js";
import {
  getCurrentWeeklyNotePath,
  getCurrentDayOfWeek,
  getISOWeek,
  getISOWeekYear,
} from "../week-utils.js";

const Args = z.object({
  // No arguments - always returns current week's note URI
});
type Args = z.infer<typeof Args>;

export const GetWeeklyNote = {
  definition: {
    name: "GetWeeklyNote",
    description:
      "Get the URI for the current week's journal note. Returns a resource link that can be read to access the note content.",
    inputSchema: z.toJSONSchema(Args),
  },

  async handler(args: unknown, context: ToolContext) {
    Args.parse(args);

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
  },
} satisfies MCPTool;
