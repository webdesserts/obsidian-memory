import { DateTime } from "luxon";

/**
 * Format ISO week date as YYYY-Www-D (e.g., 2025-W48-1)
 */
export function formatISOWeekDate(dt: DateTime): string {
  return dt.toFormat("kkkk-'W'WW-c");
}

/**
 * Get 3-letter day abbreviation (Mon, Tue, etc.)
 */
export function getDayAbbreviation(dt: DateTime): string {
  return dt.toFormat("ccc");
}

/**
 * Format time as 12-hour clock (h:MM AM/PM)
 */
export function format12HourTime(dt: DateTime): string {
  return dt.toFormat("h:mm a");
}

/**
 * Parse ISO week date string (e.g., '2025-W50-1') to DateTime
 */
export function parseISOWeekDate(isoWeekDate: string): DateTime | null {
  const parsed = DateTime.fromFormat(isoWeekDate, "kkkk-'W'WW-c");
  return parsed.isValid ? parsed : null;
}

/**
 * Parse a log entry to extract time (for sorting)
 */
export function parseEntryTime(entry: string): DateTime | null {
  const match = entry.match(/^- (.+?) –/);
  if (!match) return null;

  const timeStr = match[1];
  const parsed = DateTime.fromFormat(timeStr, "h:mm a");

  return parsed.isValid ? parsed : null;
}

/**
 * Check if a line is a valid day header (e.g., "## 2025-W50-1 (Mon)")
 * Uses Luxon parsing to validate the format instead of regex
 */
export function isDayHeader(line: string): boolean {
  // Extract ISO week date from "## 2025-W50-1 (Mon)" format
  const match = line.match(/^## (.+?) \(/);
  if (!match) return false;

  const isoWeekDate = match[1];
  const parsed = parseISOWeekDate(isoWeekDate);
  return parsed !== null;
}

/**
 * Find a day section in log file lines
 * Returns the start and end line indices, or null if not found
 */
export function findDaySection(
  lines: string[],
  dayHeader: string
): { start: number; end: number } | null {
  let sectionStart = -1;
  let sectionEnd = -1;

  // Find the section start
  for (let i = 0; i < lines.length; i++) {
    if (lines[i] === dayHeader) {
      sectionStart = i;
      break;
    }
  }

  if (sectionStart === -1) {
    return null;
  }

  // Find the section end (next valid day header or end of file)
  for (let j = sectionStart + 1; j < lines.length; j++) {
    if (isDayHeader(lines[j])) {
      sectionEnd = j;
      break;
    }
  }

  if (sectionEnd === -1) {
    sectionEnd = lines.length;
  }

  return { start: sectionStart, end: sectionEnd };
}
