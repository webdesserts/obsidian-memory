import fs from "fs/promises";
import path from "path";
import { DateTime } from "luxon";
import { z } from "zod";
import type { McpServer } from "../server.js";
import type { ToolContext } from "../types.js";

/**
 * Format ISO week date as YYYY-Www-D (e.g., 2025-W48-1)
 */
function formatISOWeekDate(dt: DateTime): string {
  return dt.toFormat("kkkk-'W'WW-c");
}

/**
 * Get 3-letter day abbreviation (Mon, Tue, etc.)
 */
function getDayAbbreviation(dt: DateTime): string {
  return dt.toFormat("ccc");
}

/**
 * Format time as 12-hour clock (h:MM AM/PM)
 */
function format12HourTime(dt: DateTime): string {
  return dt.toFormat("h:mm a");
}

/**
 * Parse a log entry to extract time (for sorting)
 */
function parseEntryTime(entry: string): DateTime | null {
  const match = entry.match(/^- (.+?) –/);
  if (!match) return null;

  const timeStr = match[1];
  const parsed = DateTime.fromFormat(timeStr, "h:mm a");

  return parsed.isValid ? parsed : null;
}

/**
 * Add a new entry to the log file, organizing by day and sorting chronologically
 */
export async function addLog(
  logPath: string,
  time: DateTime,
  entry: string
): Promise<{ isoWeekDate: string; timeStr: string }> {
  const isoWeekDate = formatISOWeekDate(time);
  const dayAbbrev = getDayAbbreviation(time);
  const timeStr = format12HourTime(time);

  // Format the new entry
  const bulletContent = entry.startsWith("-") ? entry.slice(1).trim() : entry;
  const newEntry = `- ${timeStr} – ${bulletContent}`;

  // Read existing log content
  let logContent = "";
  try {
    logContent = await fs.readFile(logPath, "utf-8");
  } catch (error) {
    // File doesn't exist, will be created
  }

  // Parse log into sections
  const dayHeader = `## ${isoWeekDate} (${dayAbbrev})`;
  const lines = logContent.split("\n");

  let sectionIndex = -1;
  let insertIndex = -1;

  // Find the section for this day
  for (let i = 0; i < lines.length; i++) {
    if (lines[i] === dayHeader) {
      sectionIndex = i;
      break;
    }
  }

  if (sectionIndex === -1) {
    // Day section doesn't exist - add at the end
    const newSection = logContent
      ? `\n${dayHeader}\n\n${newEntry}\n`
      : `${dayHeader}\n\n${newEntry}\n`;
    await fs.appendFile(logPath, newSection);
  } else {
    // Day section exists - find insertion point for chronological order
    const entries: Array<{
      line: string;
      time: DateTime | null;
      index: number;
    }> = [];
    let currentIndex = sectionIndex + 1;

    // Skip blank lines after header
    while (currentIndex < lines.length && lines[currentIndex].trim() === "") {
      currentIndex++;
    }

    // Collect all entries in this section
    while (
      currentIndex < lines.length &&
      !lines[currentIndex].startsWith("##")
    ) {
      const line = lines[currentIndex];
      if (line.startsWith("-")) {
        entries.push({
          line,
          time: parseEntryTime(line),
          index: currentIndex,
        });
      }
      currentIndex++;
    }

    // Find where to insert based on time
    insertIndex = sectionIndex + 1;

    // Skip blank lines
    while (insertIndex < lines.length && lines[insertIndex].trim() === "") {
      insertIndex++;
    }

    // Find chronological position
    for (const entry of entries) {
      if (entry.time && entry.time > time) {
        insertIndex = entry.index;
        break;
      }
      insertIndex = entry.index + 1;
    }

    // Insert the new entry
    lines.splice(insertIndex, 0, newEntry);
    await fs.writeFile(logPath, lines.join("\n"));
  }

  return { isoWeekDate, timeStr };
}

/**
 * Log Tool
 *
 * Append a timestamped entry to Log.md for temporal memory tracking.
 * Automatically organizes entries by ISO week date with day headers.
 */
export function registerLog(server: McpServer, context: ToolContext) {
  server.registerTool(
    "Log",
    {
      title: "Log Timeline Entry",
      description:
        "Append a timestamped entry to Log.md for active work state and debugging context tracking. " +
        "Records chronological session activity - what happened when. " +
        "The tool automatically adds timestamps and organizes entries by day. " +
        "Use this for tracking work in progress, debugging steps, state changes, and decisions made during active work.",
      inputSchema: {
        content: z
          .string()
          .describe(
            "Timeline entry content (single bullet point). Tool adds timestamp and day headers automatically. " +
              "Tag work items with associated jira tickets or github issues when relevant."
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
      const logPath = path.join(vaultPath, "Log.md");
      const now = DateTime.local();

      const { isoWeekDate, timeStr } = await addLog(logPath, now, content);

      return {
        content: [
          {
            type: "text",
            text: `Logged at ${isoWeekDate} ${timeStr}`,
          },
        ],
        structuredContent: {
          isoWeekDate,
          time: timeStr,
        },
      };
    }
  );
}
