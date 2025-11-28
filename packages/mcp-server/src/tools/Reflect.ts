import { z } from "zod";
import type { McpServer } from "../server.js";
import type { ToolContext } from "../types.js";
import { buildReflectPrompt } from "../prompts/reflectPrompt.js";

/**
 * Reflect Tool
 *
 * Review Log.md and Working Memory.md and consolidate content into permanent notes.
 * Returns consolidation instructions that Claude should follow.
 */
export function registerReflect(
  server: McpServer,
  context: ToolContext
) {
  server.registerTool(
    "Reflect",
    {
      title: "Reflect and Consolidate Memory",
      description:
        "Review active context (Log.md, Working Memory.md, current weekly journal, project notes) and consolidate content into permanent storage. " +
        "Optimizes token usage by keeping active/relevant work accessible while compressing or archiving finished work. " +
        "Applies information lifecycle: active work = keep lean, shipped/merged = compress and archive. " +
        "Returns detailed consolidation instructions.",
      inputSchema: {
        includePrivate: z
          .boolean()
          .optional()
          .describe("Include private notes in reflection (default: false)"),
      },
      annotations: {
        readOnlyHint: false,
        destructiveHint: false,
        openWorldHint: false,
      },
    },
    async ({ includePrivate = false }) => {
      console.error(
        `[Reflect] Triggering reflection (includePrivate: ${includePrivate})`
      );

      // Get current date info
      const now = new Date();
      const weekNumber = getWeekNumber(now);
      const dayOfWeek = getDayOfWeek(now);
      const year = now.getFullYear();
      const weeklyNotePath = `journal/${year}-w${weekNumber
        .toString()
        .padStart(2, "0")}.md`;

      const promptText = buildReflectPrompt({
        weeklyNotePath,
        dayOfWeek,
        weekNumber,
        includePrivate,
      });

      return {
        content: [
          {
            type: "text",
            text: promptText,
            annotations: {
              audience: ["assistant"],
              priority: 0.9,
            },
          },
        ],
      };
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
