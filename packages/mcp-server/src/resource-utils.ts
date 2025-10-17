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
  noteRef: string;
  /** Tool context for vault access */
  context: Pick<ToolContext, "vaultPath" | "graphIndex">;
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
 * Handles smart lookup across common locations and graph index
 */
export async function resolveNotePath(
  options: ResolveNotePathOptions
): Promise<string> {
  const { noteRef, context } = options;
  const { vaultPath, graphIndex } = context;

  // If it's a memory: URI, extract the pathname
  if (noteRef.startsWith("memory:")) {
    const url = new URL(noteRef);
    return url.pathname;
  }

  // Normalize the note reference (handles [[Note]], paths, etc.)
  const notePath = normalizeNoteReference(noteRef);
  const noteNameOnly = extractNoteName(notePath);

  // If path includes a folder, use it directly
  if (notePath.includes("/")) {
    return notePath;
  }

  // Smart lookup: try common locations in priority order
  const searchPaths = generateSearchPaths(noteNameOnly, false);

  for (const searchPath of searchPaths) {
    const notePathWithExt = ensureMarkdownExtension(searchPath);
    const absolutePath = validatePath(vaultPath, notePathWithExt);

    if (await fileExists(absolutePath)) {
      return searchPath;
    }
  }

  // Fall back to graph index if not found in standard paths
  const indexPath = graphIndex.getNotePath(noteNameOnly);
  if (indexPath) {
    return indexPath;
  }

  // Return the original path if nothing found (will handle as non-existent)
  return notePath;
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
  const notePath = await resolveNotePath({ noteRef, context });
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
