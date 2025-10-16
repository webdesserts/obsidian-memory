/**
 * Utilities for building MCP resource responses from note data
 */

import { ToolResponse } from "./types.js";
import { FileOperations } from "../file-operations.js";
import { extractNoteName } from "@obsidian-memory/utils";

export interface ResourceAnnotations {
  /** Intended audience for the resource */
  audience?: string[];
  /** Priority level (0.0-1.0) */
  priority?: number;
}

export interface ReadNoteResourceOptions {
  /** Relative path within vault (e.g., "knowledge/Note Name") */
  notePath: string;
  /** Vault name for obsidian:// URI */
  vaultName: string;
  /** Absolute vault path */
  vaultPath: string;
  /** File operations instance */
  fileOps: FileOperations;
  /** Custom annotations (default: audience both, priority 0.5) */
  annotations?: ResourceAnnotations;
}

/**
 * Read a note and build a standardized MCP tool response with embedded resource
 * Handles the complete flow: read file -> parse -> build response
 */
export async function readNoteResource(
  options: ReadNoteResourceOptions
): Promise<ToolResponse> {
  const {
    notePath,
    vaultName,
    vaultPath,
    fileOps,
    annotations = {},
  } = options;

  const noteName = extractNoteName(notePath);

  // Try to read the note
  let exists = false;
  let content = "";
  let frontmatter: Record<string, unknown> | undefined;

  try {
    const result = await fileOps.readNote(notePath);
    exists = true;
    content = result.content;
    frontmatter = result.frontmatter;
  } catch (error) {
    // Note doesn't exist - leave exists as false
  }

  // Build obsidian:// URI
  const obsidianUri = `obsidian://open?vault=${encodeURIComponent(vaultName)}&file=${encodeURIComponent(notePath)}`;

  // Build structured content with machine-readable metadata
  const structuredContent: Record<string, unknown> = {
    exists,
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
          uri: `memory://${notePath}`,
          title: noteName,
          mimeType: "text/markdown",
          text: exists ? content : null,
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
