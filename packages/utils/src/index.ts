/**
 * Obsidian Memory Utilities
 *
 * Shared utilities for working with Obsidian vaults:
 * - Wiki link parsing
 * - Path validation and helpers
 *
 * @packageDocumentation
 */

// Wiki link parsing exports
export type { WikiLink } from "./wiki-links.js";
export { parseWikiLinks, extractLinkedNotes } from "./wiki-links.js";

// Path utility exports
export { validatePath, fileExists, ensureMarkdownExtension } from "./path.js";
