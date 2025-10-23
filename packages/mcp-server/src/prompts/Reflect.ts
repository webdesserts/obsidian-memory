/**
 * Reflect Prompt
 *
 * MCP prompt for reflecting on Working Memory and consolidating content.
 */

import type { McpServer } from "../server.js";
import type { ToolContext } from "../types.js";
import { z } from "zod";
import { generateReflectPrompt } from "./generateReflectPrompt.js";

/**
 * Register reflect prompt with the MCP server
 */
export function registerReflectPrompt(server: McpServer, context: ToolContext) {
  // Register reflect prompt
  server.registerPrompt(
    "reflect",
    {
      title: "Reflect on Working Memory",
      description:
        "Review Working Memory and consolidate content into permanent notes (knowledge notes, project notes, weekly journal)",
      argsSchema: {
        includePrivate: z
          .boolean()
          .optional()
          .describe("Include private notes in reflection (default: false)"),
      },
    },
    async ({ includePrivate = false }) => {
      const { memorySystem, fileOps } = context;

      // Get Working Memory content
      const workingMemory = memorySystem.getWorkingMemory() || "";

      // Get current date info
      const now = new Date();
      const weekNumber = getWeekNumber(now);
      const year = now.getFullYear();
      const dayOfWeek = getDayOfWeek(now);
      const weeklyNotePath = `journal/${year}-w${weekNumber.toString().padStart(2, "0")}.md`;

      // Generate the prompt
      return generateReflectPrompt({
        workingMemoryContent: workingMemory,
        weeklyNotePath,
        currentWeekNumber: weekNumber,
        currentDayOfWeek: dayOfWeek,
        includePrivate,
      });
    }
  );
}

/**
 * Get ISO week number for a date
 * https://en.wikipedia.org/wiki/ISO_week_date
 */
function getWeekNumber(date: Date): number {
  const target = new Date(date.valueOf());
  const dayNumber = (date.getDay() + 6) % 7;
  target.setDate(target.getDate() - dayNumber + 3);
  const firstThursday = target.valueOf();
  target.setMonth(0, 1);
  if (target.getDay() !== 4) {
    target.setMonth(0, 1 + ((4 - target.getDay() + 7) % 7));
  }
  return 1 + Math.ceil((firstThursday - target.valueOf()) / 604800000);
}

/**
 * Get day of week name
 */
function getDayOfWeek(date: Date): string {
  const days = ["Sunday", "Monday", "Tuesday", "Wednesday", "Thursday", "Friday", "Saturday"];
  return days[date.getDay()];
}
