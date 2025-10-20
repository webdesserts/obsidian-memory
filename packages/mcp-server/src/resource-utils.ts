/**
 * Utilities for building MCP resource responses from note data
 */

import { ToolResponse, ToolContext } from "./types.js";
import { FileOperations } from "./file-operations.js";
import {
  extractNoteName,
  normalizeNoteReference,
  generateSearchPaths,
  validatePath,
  fileExists,
  ensureMarkdownExtension,
} from "@obsidian-memory/utils";

export interface ResourceAnnotations {
  /** Intended audience for the resource */
  audience?: string[];
  /** Priority level (0.0-1.0) */
  priority?: number;
}

export interface ResolveNotePathOptions {
  /** Note reference (supports: "Note Name", "knowledge/Note", "memory:Note", "[[Note]]") */
  note: string;
  /** Tool context for vault access */
  context: Pick<ToolContext, "vaultPath">;
}

export interface ReadNoteResourceOptions {
  /** Note reference (supports: "Note Name", "knowledge/Note", "memory:Note", "[[Note]]") */
  noteRef: string;
  /** Tool context for vault access */
  context: ToolContext;
  /** Custom annotations (default: audience both, priority 0.5) */
  annotations?: ResourceAnnotations;
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
  let notePath = normalizeNoteReference(note);

  // Extract just the note name and any parent path
  const noteNameOnly = extractNoteName(notePath);
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

/**
 * Read a note and build a standardized MCP tool response with embedded resource
 * Handles the complete flow: resolve path -> read file -> parse -> build response
 */
export async function readNoteResource(
  options: ReadNoteResourceOptions
): Promise<ToolResponse> {
  const { noteRef, context, annotations = {} } = options;
  const { vaultName, vaultPath, fileOps } = context;

  // Resolve the note reference to a vault-relative path
  const notePath = await resolveNotePath({ note: noteRef, context });
  const noteName = extractNoteName(notePath);

  // Build obsidian:// URI
  const obsidianUri = `obsidian://open?vault=${encodeURIComponent(
    vaultName
  )}&file=${encodeURIComponent(notePath)}`;

  // Try to read the note
  let rawContent: string;
  let frontmatter: Record<string, unknown> | undefined;

  try {
    const result = await fileOps.readNote(notePath);
    rawContent = result.rawContent;
    frontmatter = result.frontmatter;
  } catch (error) {
    // Note doesn't exist - return error response with resolution metadata
    const filePath = `${vaultPath}/${notePath}.md`;
    const memoryUri = `memory:${notePath}`;

    return {
      content: [
        {
          type: "text",
          text: `Note not found: ${noteName}

This note doesn't exist yet. You can create it at:
- File path: ${filePath}
- Memory URI: ${memoryUri}
- Obsidian URI: ${obsidianUri}

Use the Write tool with the file path to create this note.`,
        },
      ],
      structuredContent: {
        noteName,
        resolvedPath: notePath,
        filePath,
        memoryUri,
        obsidianUri,
      },
      isError: true,
    };
  }

  // Build structured content with machine-readable metadata
  const structuredContent: Record<string, unknown> = {
    noteName,
    path: notePath,
    filePath: `${vaultPath}/${notePath}.md`,
    obsidianUri,
  };

  // Only include frontmatter if it exists
  if (frontmatter) {
    structuredContent.frontmatter = frontmatter;
  }

  // Return as embedded resource
  return {
    content: [
      {
        type: "resource",
        resource: {
          uri: `memory:${notePath}`,
          title: noteName,
          mimeType: "text/markdown",
          text: rawContent,
          annotations: {
            audience: annotations.audience ?? ["user", "assistant"],
            priority: annotations.priority ?? 0.5,
          },
        },
      },
    ],
    structuredContent,
  };
}
