/**
 * Utilities for working with weekly notes
 * Uses ISO 8601 week date system (same as Obsidian)
 */

/**
 * Get the ISO week number for a date
 * Week 1 is the week containing the first Thursday of the year
 */
function getISOWeek(date: Date): number {
  const target = new Date(date.valueOf());
  const dayNr = (date.getDay() + 6) % 7;
  target.setDate(target.getDate() - dayNr + 3);
  const firstThursday = target.valueOf();
  target.setMonth(0, 1);
  if (target.getDay() !== 4) {
    target.setMonth(0, 1 + ((4 - target.getDay()) + 7) % 7);
  }
  return 1 + Math.ceil((firstThursday - target.valueOf()) / 604800000);
}

/**
 * Get the ISO week year for a date
 * The week year can differ from the calendar year near year boundaries
 */
function getISOWeekYear(date: Date): number {
  const target = new Date(date.valueOf());
  target.setDate(target.getDate() + 3 - (date.getDay() + 6) % 7);
  return target.getFullYear();
}

/**
 * Get the current weekly note URI
 * Format: memory:journal/YYYY-wWW (e.g., memory:journal/2025-w42)
 */
export function getCurrentWeeklyNotePath(): string {
  const now = new Date();
  const year = getISOWeekYear(now);
  const week = getISOWeek(now);
  return `memory:journal/${year}-w${String(week).padStart(2, '0')}`;
}

/**
 * Get the current day of week
 * Returns: Monday, Tuesday, Wednesday, Thursday, Friday, Saturday, Sunday
 */
export function getCurrentDayOfWeek(): string {
  const days = ['Sunday', 'Monday', 'Tuesday', 'Wednesday', 'Thursday', 'Friday', 'Saturday'];
  return days[new Date().getDay()];
}

/**
 * Get weekly note path for a specific date
 */
export function getWeeklyNotePathForDate(date: Date): string {
  const year = getISOWeekYear(date);
  const week = getISOWeek(date);
  return `journal/${year}-w${String(week).padStart(2, '0')}`;
}
