import { z } from "zod";
import { ToolContext, MCPTool } from "./types.js";
import { extractNoteName } from "@obsidian-memory/utils";
import { readNoteResource } from "./resource-utils.js";

const Args = z.object({
  note: z.string().describe(
    "Note name or path. Supports: 'Note Name', 'Note Name.md', 'knowledge/Note Name', 'memory:knowledge/Note Name'"
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

    // Read note and build resource response (handles path resolution internally)
    const response = await readNoteResource({
      noteRef: note,
      context,
    });

    // Log note access for usage statistics (if it exists)
    if (response.structuredContent?.exists) {
      const noteName = extractNoteName(note);
      context.memorySystem.logAccess(noteName, "ReadNote");
    }

    return response;
  },
} satisfies MCPTool;
