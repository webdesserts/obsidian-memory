import { z } from "zod";
import { ToolContext, MCPTool } from "./types.js";
import {
  normalizeNoteReference,
  extractNoteName,
  generateSearchPaths,
} from "@obsidian-memory/utils";

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
    const { fileOps, graphIndex, memorySystem, vaultPath } = context;

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

    // Read the note
    const result = await fileOps.readNote(finalPath);

    // Log note access for usage statistics
    memorySystem.logAccess(noteNameOnly, "ReadNote");

    // Build metadata
    const metadata = {
      noteName: noteNameOnly,
      memoryUri: `memory://${finalPath}`,
      filePath: `${vaultPath}/${finalPath}.md`,
    };

    // Build response with metadata first, then content
    let response = `\`\`\`json\n${JSON.stringify(metadata, null, 2)}\n\`\`\`\n\n`;

    if (result.frontmatter) {
      response += `---\nFrontmatter:\n${JSON.stringify(
        result.frontmatter,
        null,
        2
      )}\n---\n\n`;
    }

    response += result.content;

    return {
      content: [{ type: "text", text: response }],
    };
  },
} satisfies MCPTool;
