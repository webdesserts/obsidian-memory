import { z } from "zod";
import { ToolContext, MCPTool } from "./types.js";
import { getCurrentWeeklyNotePath, getCurrentDayOfWeek } from "../week-utils.js";
import { readNoteResource } from "./resource-utils.js";

const Args = z.object({
  // No arguments - always returns current week's note
});
type Args = z.infer<typeof Args>;

export const ReadWeeklyNote = {
  definition: {
    name: "ReadWeeklyNote",
    description: "Read the current week's journal note. Returns the full note content with metadata including the current day of the week.",
    inputSchema: z.toJSONSchema(Args),
  },

  async handler(args: unknown, context: ToolContext) {
    Args.parse(args);

    // Get current weekly note URI and current day
    const weeklyNoteRef = getCurrentWeeklyNotePath();
    const currentDay = getCurrentDayOfWeek();

    // Read note and build resource response
    const response = await readNoteResource({
      noteRef: weeklyNoteRef,
      context,
      annotations: {
        priority: 1.0, // High priority - user's active work hub
      },
    });

    // Log access for usage statistics (if it exists)
    if (response.structuredContent?.exists) {
      // Extract path from memory: URI for logging
      const url = new URL(weeklyNoteRef);
      context.memorySystem.logAccess(url.pathname, "ReadWeeklyNote");
    }

    // Add current day to structured content
    if (response.structuredContent) {
      response.structuredContent.currentDay = currentDay;
    }

    return response;
  },
} satisfies MCPTool;
