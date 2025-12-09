import fs from "fs/promises";
import path from "path";
import { DateTime } from "luxon";
import { z } from "zod";
import type { McpServer } from "../server.js";
import type { ToolContext } from "../types.js";
import {
  formatISOWeekDate,
  getDayAbbreviation,
  parseISOWeekDate,
  findDaySection,
} from "../utils/log-format.js";

/**
 * Replace an entire day section in the log with new entries
 * Pass an empty object to delete the entire day section
 */
export async function writeLogs(
  logPath: string,
  isoWeekDate: string,
  entries: Record<string, string>
): Promise<{ count: number; errors: string[]; deleted: boolean }> {
  const errors: string[] = [];

  // Validate and parse ISO week date
  const dayDateTime = parseISOWeekDate(isoWeekDate);
  if (!dayDateTime) {
    errors.push(
      `Invalid ISO week date format: '${isoWeekDate}'. Expected format: YYYY-Www-D (e.g., 2025-W50-1)`
    );
    return { count: 0, errors, deleted: false };
  }

  const dayAbbrev = getDayAbbreviation(dayDateTime);
  const dayHeader = `## ${isoWeekDate} (${dayAbbrev})`;

  // Handle empty object - delete the entire day section
  if (Object.keys(entries).length === 0) {
    // Read existing log content
    let logContent = "";
    try {
      logContent = await fs.readFile(logPath, "utf-8");
    } catch (error) {
      // File doesn't exist - nothing to delete
      return { count: 0, errors: [], deleted: false };
    }

    // Find and remove the day section
    const lines = logContent.split("\n");
    const section = findDaySection(lines, dayHeader);

    if (!section) {
      // Day section doesn't exist - nothing to delete
      return { count: 0, errors: [], deleted: false };
    }

    // Remove the section
    lines.splice(section.start, section.end - section.start);

    // Clean up extra blank lines
    const updatedContent = lines.join("\n").replace(/\n{3,}/g, "\n\n");
    await fs.writeFile(logPath, updatedContent);

    return { count: 0, errors: [], deleted: true };
  }

  // Parse and sort entries by time
  const parsedEntries: Array<{ time: DateTime; timeStr: string; message: string }> =
    [];

  for (const [timeStr, message] of Object.entries(entries)) {
    const parsed = DateTime.fromFormat(timeStr, "h:mm a");
    if (!parsed.isValid) {
      errors.push(
        `Invalid time format: '${timeStr}'. Expected format: h:mm AM/PM (e.g., 9:00 AM, 2:30 PM)`
      );
      continue;
    }

    parsedEntries.push({ time: parsed, timeStr, message });
  }

  // If there were parsing errors, return early
  if (errors.length > 0) {
    return { count: 0, errors, deleted: false };
  }

  // Sort chronologically
  parsedEntries.sort((a, b) => a.time.toMillis() - b.time.toMillis());

  // Generate the new day section
  const entryLines = parsedEntries.map(
    (entry) => `- ${entry.timeStr} – ${entry.message}`
  );
  const newSection = [dayHeader, "", ...entryLines, ""].join("\n");

  // Read existing log content
  let logContent = "";
  try {
    logContent = await fs.readFile(logPath, "utf-8");
  } catch (error) {
    // File doesn't exist, create it with just this section
    await fs.writeFile(logPath, newSection);
    return { count: parsedEntries.length, errors: [], deleted: false };
  }

  // Find and replace the day section
  const lines = logContent.split("\n");
  const section = findDaySection(lines, dayHeader);

  if (!section) {
    // Day section doesn't exist - append at the end
    const updatedContent = logContent.endsWith("\n")
      ? logContent + "\n" + newSection
      : logContent + "\n\n" + newSection;
    await fs.writeFile(logPath, updatedContent);
  } else {
    // Replace the existing section
    lines.splice(section.start, section.end - section.start, ...newSection.split("\n"));
    await fs.writeFile(logPath, lines.join("\n"));
  }

  return { count: parsedEntries.length, errors: [], deleted: false };
}

/**
 * WriteLogs Tool
 *
 * Replace an entire day's log entries in Log.md with consolidated/compacted entries.
 * Use this ONLY during memory consolidation when you need to rewrite or summarize a day's logs.
 * For adding new entries during active work, use the Log tool instead.
 */
export function registerWriteLogs(server: McpServer, context: ToolContext) {
  server.registerTool(
    "WriteLogs",
    {
      title: "Rewrite Day's Log Entries",
      description:
        "Replace an entire day's log entries with consolidated/compacted entries. " +
        "Use this ONLY during memory consolidation to rewrite or summarize a day's logs. " +
        "For adding new entries during active work, use the Log tool instead (it's simpler and doesn't require reading the log first). " +
        "This tool automatically formats entries with correct timestamps, en-dashes, and chronological sorting. " +
        "Pass an empty object to delete the entire day section (header and all entries).",
      inputSchema: {
        isoWeekDate: z
          .string()
          .describe(
            "ISO week date in YYYY-Www-D format (e.g., '2025-W50-1' for Monday of week 50). " +
              "Week starts on Monday (1=Mon, 7=Sun)."
          ),
        entries: z
          .record(z.string(), z.string())
          .describe(
            "Object mapping time strings to log messages. " +
              "Keys: '9:00 AM', '2:30 PM', etc. (12-hour format with AM/PM). " +
              "Values: Log entry content. " +
              "Example: { '9:00 AM': 'Started investigation', '2:30 PM': 'Fixed bug #123' }"
          ),
      },
      annotations: {
        readOnlyHint: false,
        destructiveHint: true, // Replaces entire day section
        openWorldHint: false,
      },
    },
    async ({ isoWeekDate, entries }) => {
      const { vaultPath } = context;
      const logPath = path.join(vaultPath, "Log.md");

      const { count, errors, deleted } = await writeLogs(logPath, isoWeekDate, entries);

      if (errors.length > 0) {
        return {
          content: [
            {
              type: "text",
              text: `Failed to write logs:\n${errors.map((e) => `- ${e}`).join("\n")}`,
            },
          ],
          isError: true,
        };
      }

      if (deleted) {
        return {
          content: [
            {
              type: "text",
              text: `Deleted day section ${isoWeekDate}`,
            },
          ],
          structuredContent: {
            isoWeekDate,
            deleted: true,
          },
        };
      }

      return {
        content: [
          {
            type: "text",
            text: `Successfully wrote ${count} log entries for ${isoWeekDate}`,
          },
        ],
        structuredContent: {
          isoWeekDate,
          count,
        },
      };
    }
  );
}
