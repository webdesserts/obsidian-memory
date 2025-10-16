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
    const { fileOps, vaultPath, vaultName, memorySystem } = context;

    // Get current weekly note path
    const weeklyNotePath = getCurrentWeeklyNotePath();
    const currentDay = getCurrentDayOfWeek();

    // Read note and build resource response
    const response = await readNoteResource({
      notePath: weeklyNotePath,
      vaultName,
      vaultPath,
      fileOps,
      annotations: {
        priority: 1.0, // High priority - user's active work hub
      },
    });

    // Log access for usage statistics (if it exists)
    if (response.structuredContent?.exists) {
      memorySystem.logAccess(weeklyNotePath, "ReadWeeklyNote");
    }

    // Add current day to structured content
    if (response.structuredContent) {
      response.structuredContent.currentDay = currentDay;
    }

    return response;
  },
} satisfies MCPTool;
