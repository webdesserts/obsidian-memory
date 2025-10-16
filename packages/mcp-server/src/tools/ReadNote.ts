import { z } from "zod";
import { ToolContext, MCPTool } from "./types.js";
import {
  normalizeNoteReference,
  extractNoteName,
  generateSearchPaths,
} from "@obsidian-memory/utils";
import { readNoteResource } from "./resource-utils.js";

const Args = z.object({
  note: z.string().describe(
    "Note name or path. Supports: 'Note Name', 'Note Name.md', 'knowledge/Note Name', 'memory://knowledge/Note Name'"
  ),
});
type Args = z.infer<typeof Args>;

export const ReadNote = {
  definition: {
    name: "ReadNote",
    description: "Read the content of a note from the vault",
    inputSchema: z.toJSONSchema(Args),
  },

  async handler(args: unknown, context: ToolContext) {
    const { note } = Args.parse(args);
    const { fileOps, graphIndex, memorySystem, vaultPath, vaultName } = context;

    // Normalize the note reference
    const notePath = normalizeNoteReference(note);
    const noteNameOnly = extractNoteName(notePath);

    // Determine final path using smart lookup
    let finalPath: string = notePath; // Default to provided path

    // If path includes a folder, use it directly
    if (notePath.includes("/")) {
      finalPath = notePath;
    } else {
      // Smart lookup: try common locations in priority order
      const searchPaths = generateSearchPaths(noteNameOnly, false);

      let found = false;
      // Try each path until we find one that exists
      for (const searchPath of searchPaths) {
        try {
          await fileOps.readNote(searchPath);
          finalPath = searchPath;
          found = true;
          break;
        } catch {
          // Continue to next path
        }
      }

      // Fall back to graph index if not found in standard paths
      if (!found) {
        const indexPath = graphIndex.getNotePath(noteNameOnly);
        if (indexPath) {
          finalPath = indexPath;
        }
      }
    }

    // Read note and build resource response
    const response = await readNoteResource({
      notePath: finalPath,
      vaultName,
      vaultPath,
      fileOps,
    });

    // Log note access for usage statistics (if it exists)
    if (response.structuredContent?.exists) {
      memorySystem.logAccess(noteNameOnly, "ReadNote");
    }

    return response;
  },
} satisfies MCPTool;
