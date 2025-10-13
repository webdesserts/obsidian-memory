/**
 * Path resolution utilities for Obsidian notes
 *
 * Handles ambiguous note names and provides consistent resolution logic
 * across different tools (read_note, graph navigation, etc.)
 */

export interface PathResolutionOptions {
  /** Whether to include private folder in search */
  includePrivate?: boolean;
  /** Custom priority order for path resolution */
  priorityOrder?: ((path: string) => boolean)[];
}

/**
 * Default priority order for resolving ambiguous note paths
 * Priority: root → knowledge/ → journal/ → others → private/
 */
export const DEFAULT_PRIORITY_ORDER: ((path: string) => boolean)[] = [
  (p: string) => !p.includes("/"), // Root level first
  (p: string) => p.startsWith("knowledge/"),
  (p: string) => p.startsWith("journal/"),
  (p: string) => !p.startsWith("private/"), // Non-private before private
  () => true, // Any remaining (including private)
];

/**
 * Resolve a note path from available options using priority order
 *
 * @param availablePaths - Array of paths to choose from
 * @param options - Resolution options
 * @returns The best matching path, or undefined if none found
 */
export function resolveNotePath(
  availablePaths: string[],
  options: PathResolutionOptions = {}
): string | undefined {
  if (availablePaths.length === 0) return undefined;
  if (availablePaths.length === 1) return availablePaths[0];

  const { includePrivate = false, priorityOrder = DEFAULT_PRIORITY_ORDER } = options;

  // Filter out private paths if not included
  let paths = availablePaths;
  if (!includePrivate) {
    const nonPrivatePaths = paths.filter((p) => !p.startsWith("private/"));
    // Only filter if there are non-private alternatives
    if (nonPrivatePaths.length > 0) {
      paths = nonPrivatePaths;
    }
  }

  // Apply priority order
  for (const predicate of priorityOrder) {
    const match = paths.find(predicate);
    if (match) return match;
  }

  // Fallback to first path
  return paths[0];
}

/**
 * Common search paths for note lookup (relative to vault root)
 */
export const COMMON_SEARCH_PATHS = [
  "", // Root
  "knowledge",
  "journal",
];

/**
 * Generate search paths for a note name
 *
 * @param noteName - The note name to search for
 * @param includePrivate - Whether to include private folder
 * @returns Array of paths to try (without .md extension)
 */
export function generateSearchPaths(
  noteName: string,
  includePrivate: boolean = false
): string[] {
  const paths: string[] = [];

  // Add common paths
  for (const folder of COMMON_SEARCH_PATHS) {
    paths.push(folder ? `${folder}/${noteName}` : noteName);
  }

  // Add private path if requested
  if (includePrivate) {
    paths.push(`private/${noteName}`);
  }

  return paths;
}

/**
 * Normalize a note reference (strip memory:// prefix and .md extension)
 *
 * @param noteRef - Note reference (can be name, path, or memory:// URI)
 * @returns Normalized path without extension
 */
export function normalizeNoteReference(noteRef: string): string {
  let normalized = noteRef;

  // Strip memory:// prefix if present
  if (normalized.startsWith("memory://")) {
    normalized = normalized.slice(9);
  }

  // Strip .md extension if present
  if (normalized.endsWith(".md")) {
    normalized = normalized.slice(0, -3);
  }

  return normalized;
}

/**
 * Extract note name from a path (last component without extension)
 *
 * @param notePath - Path to the note
 * @returns Just the note name
 */
export function extractNoteName(notePath: string): string {
  const normalized = normalizeNoteReference(notePath);
  const parts = normalized.split("/");
  return parts[parts.length - 1];
}
