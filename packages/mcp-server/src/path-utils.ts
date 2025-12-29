/**
 * Utilities for resolving note paths
 */

import { ToolContext } from "./types.js";
import {
  extractNoteName,
  normalizeNoteReference,
  generateSearchPaths,
  ensureMarkdownExtension,
} from "@webdesserts/obsidian-memory-core";
import path from "path";
import fs from "fs/promises";

/**
 * Validates that a file path is within the vault and safe to access
 */
function validatePath(vaultPath: string, relativePath: string): string {
  // Remove leading slash if present
  const cleanPath = relativePath.startsWith("/")
    ? relativePath.slice(1)
    : relativePath;

  // Resolve absolute path
  const absolutePath = path.resolve(vaultPath, cleanPath);

  // Ensure path is within vault (prevent directory traversal)
  if (!absolutePath.startsWith(path.resolve(vaultPath))) {
    throw new Error(`Path outside vault: ${relativePath}`);
  }

  return absolutePath;
}

/**
 * Checks if a file exists
 */
async function fileExists(filePath: string): Promise<boolean> {
  try {
    await fs.access(filePath);
    return true;
  } catch {
    return false;
  }
}

export interface ResolveNotePathOptions {
  /** Note reference (supports: "Note Name", "knowledge/Note", "memory:Note", "[[Note]]") */
  note: string;
  /** Tool context for vault access */
  context: Pick<ToolContext, "vaultPath">;
}

/**
 * Resolve a note reference to a vault-relative path
 * Implements Obsidian wiki-link resolution behavior from vault root
 */
export async function resolveNotePath(
  options: ResolveNotePathOptions
): Promise<string> {
  const { note, context } = options;
  const { vaultPath } = context;

  // Normalize the reference (handles memory: URIs, [[links]], .md extensions)
  const noteRef = normalizeNoteReference(note) as { path: string; name: string };
  const notePath = noteRef.path;
  const noteNameOnly = noteRef.name;

  // Check if path contains parent folder
  const parentPath = notePath.includes("/")
    ? notePath.substring(0, notePath.lastIndexOf("/"))
    : "";

  // If we have a parent path, do hierarchical lookup within that folder
  if (parentPath) {
    const foundPath = await searchInFolder(vaultPath, parentPath, noteNameOnly);
    if (foundPath) return foundPath;
    // Not found - return original path (will be handled as non-existent)
    return notePath;
  }

  // No parent path - search from vault root using Obsidian's resolution rules
  // Priority: root first, then subdirectories alphabetically
  const searchPaths = generateSearchPaths(noteNameOnly, false);

  for (const searchPath of searchPaths) {
    const notePathWithExt = ensureMarkdownExtension(searchPath);
    const absolutePath = validatePath(vaultPath, notePathWithExt);

    if (await fileExists(absolutePath)) {
      return searchPath;
    }
  }

  // Return the original path if nothing found (will handle as non-existent)
  return notePath;
}

/**
 * Search for a note within a specific folder and its subfolders
 * Returns the first match found, searching alphabetically by subfolder
 */
async function searchInFolder(
  vaultPath: string,
  folderPath: string,
  noteName: string
): Promise<string | undefined> {
  const fs = await import("fs/promises");

  // First try the folder itself
  const directPath = `${folderPath}/${noteName}`;
  const directPathWithExt = ensureMarkdownExtension(directPath);
  const directAbsolutePath = validatePath(vaultPath, directPathWithExt);

  if (await fileExists(directAbsolutePath)) {
    return directPath;
  }

  // Then search subfolders alphabetically
  try {
    const folderAbsolutePath = validatePath(vaultPath, folderPath);
    const entries = await fs.readdir(folderAbsolutePath, {
      withFileTypes: true,
    });
    const subfolders = entries
      .filter((entry) => entry.isDirectory())
      .map((entry) => entry.name)
      .sort(); // Alphabetical order

    for (const subfolder of subfolders) {
      const subfolderPath = `${folderPath}/${subfolder}`;
      const foundPath = await searchInFolder(
        vaultPath,
        subfolderPath,
        noteName
      );
      if (foundPath) return foundPath;
    }
  } catch {
    // Folder doesn't exist or can't be read
  }

  return undefined;
}
