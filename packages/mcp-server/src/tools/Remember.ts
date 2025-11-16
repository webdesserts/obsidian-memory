import fs from "fs/promises";
import path from "path";
import type { McpServer } from "../server.js";
import type { ToolContext } from "../types.js";
import { getCurrentWeeklyNotePath } from "../week-utils.js";
import { discoverProjects } from "../projects/discovery.js";

/**
 * Remember Tool
 *
 * Load all session context files in a single call.
 * Returns Log.md, Working Memory.md, Index.md, and current weekly note.
 */
export function registerRemember(server: McpServer, context: ToolContext) {
  server.registerTool(
    "Remember",
    {
      title: "Remember Session Context",
      description:
        "Load all session context files in a single call. " +
        "Returns Log.md, Working Memory.md, Index.md, current weekly note, and discovered project notes. " +
        "Automatically discovers projects based on git remotes and directory names. " +
        "Use this at the start of every session to get complete context about recent work, " +
        "current focus, knowledge index, this week's activity, and project context.",
      inputSchema: {},
      annotations: {
        readOnlyHint: true,
        openWorldHint: false,
      },
    },
    async () => {
      const { vaultPath, graphIndex } = context;

      // Define paths to all context files
      const logPath = path.join(vaultPath, "Log.md");
      const workingMemoryPath = path.join(vaultPath, "Working Memory.md");
      const indexPath = path.join(vaultPath, "Index.md");

      // Get weekly note path
      const weeklyNoteUri = getCurrentWeeklyNotePath();
      const url = new URL(weeklyNoteUri);
      const weeklyNotePath = url.pathname;

      // Discover projects for current working directory
      const cwd = process.cwd();
      const discoveryResult = discoverProjects(cwd, graphIndex, vaultPath);

      // Read all context files in parallel
      const [logContent, workingMemoryContent, indexContent, weeklyNoteContent] =
        await Promise.all([
          fs.readFile(logPath, "utf-8").catch(() => ""),
          fs.readFile(workingMemoryPath, "utf-8").catch(() => ""),
          fs.readFile(indexPath, "utf-8").catch(() => ""),
          fs.readFile(weeklyNotePath, "utf-8").catch(() => ""),
        ]);

      // Read strict match project notes
      const projectContents = await Promise.all(
        discoveryResult.strictMatches.map(async (match) => ({
          match,
          content: await fs
            .readFile(match.metadata.filePath, "utf-8")
            .catch(() => ""),
        }))
      );

      // Build content blocks array - one resource per file
      const contentBlocks = [];

      if (logContent) {
        contentBlocks.push({
          type: "resource" as const,
          resource: {
            uri: `file://${logPath}`,
            mimeType: "text/markdown",
            text: logContent,
          },
        });
      }

      if (workingMemoryContent) {
        contentBlocks.push({
          type: "resource" as const,
          resource: {
            uri: `file://${workingMemoryPath}`,
            mimeType: "text/markdown",
            text: workingMemoryContent,
          },
        });
      }

      if (indexContent) {
        contentBlocks.push({
          type: "resource" as const,
          resource: {
            uri: `file://${indexPath}`,
            mimeType: "text/markdown",
            text: indexContent,
          },
        });
      }

      if (weeklyNoteContent) {
        contentBlocks.push({
          type: "resource" as const,
          resource: {
            uri: weeklyNoteUri,
            mimeType: "text/markdown",
            text: weeklyNoteContent,
          },
        });
      }

      // Add strictly matched project notes
      for (const { match, content } of projectContents) {
        if (content) {
          contentBlocks.push({
            type: "resource" as const,
            resource: {
              uri: `file://${match.metadata.filePath}`,
              mimeType: "text/markdown",
              text: content,
            },
          });
        }
      }

      // Build project discovery status message
      let projectStatus = "";

      if (discoveryResult.strictMatches.length > 0) {
        const projectNames = discoveryResult.strictMatches
          .map((m) => `[[${m.metadata.name}]]`)
          .join(", ");
        projectStatus = `Projects auto-loaded: ${projectNames}`;
      } else if (discoveryResult.looseMatches.length > 0) {
        // Disconnect detected
        const match = discoveryResult.looseMatches[0];
        projectStatus =
          `**Project disconnect detected**\n\n` +
          `Found project [[${match.metadata.name}]] via ${match.matchedOn} match.\n\n`;

        if (match.matchedOn === "old_remote") {
          projectStatus +=
            `Current remote: ${discoveryResult.gitRemotes[0] || "unknown"}\n` +
            `Note's expected remotes: ${match.metadata.remotes?.join(", ") || "none"}\n\n` +
            `The remote has changed. Update the project note's frontmatter:\n` +
            `1. Move old remote to old_remotes array\n` +
            `2. Add new remote to remotes array\n` +
            `3. Use UpdateFrontmatter tool or edit the file directly`;
        } else if (match.matchedOn === "old_slug") {
          projectStatus +=
            `Current directory: ${path.basename(cwd)}\n` +
            `Note's expected slug: ${match.metadata.slug || "none"}\n\n` +
            `The directory name has changed. Update the project note's frontmatter:\n` +
            `1. Move old slug to old_slugs array\n` +
            `2. Update slug to match current directory name\n` +
            `3. Use UpdateFrontmatter tool or edit the file directly`;
        }
      } else if (discoveryResult.suggestions.length > 0) {
        // No match, but found similar projects
        const suggestions = discoveryResult.suggestions
          .map((p) => `- [[${p.name}]]`)
          .join("\n");
        projectStatus =
          `**No project found**\n\n` +
          `Directory: ${path.basename(cwd)}\n` +
          `Git remotes: ${discoveryResult.gitRemotes.join(", ") || "none"}\n\n` +
          `Similar projects found:\n${suggestions}\n\n` +
          `Is this one of these existing projects, or a new project?\n` +
          `- To link to existing: Update project frontmatter with current remote/slug\n` +
          `- To create new: Write a new note in projects/ folder with appropriate frontmatter`;
      } else {
        // No match and no suggestions
        projectStatus =
          `**No project found**\n\n` +
          `Directory: ${path.basename(cwd)}\n` +
          `Git remotes: ${discoveryResult.gitRemotes.join(", ") || "none"}\n\n` +
          `Create a new project note in projects/ folder with frontmatter:\n` +
          `\`\`\`yaml\n` +
          `---\n` +
          `type: project\n` +
          (discoveryResult.gitRemotes.length > 0
            ? `remotes:\n  - ${discoveryResult.gitRemotes[0]}\n`
            : `slug: ${path.basename(cwd).toLowerCase()}\n`) +
          `---\n` +
          `\`\`\``;
      }

      // Add project status as text content if not empty
      if (projectStatus) {
        contentBlocks.push({
          type: "text" as const,
          text: projectStatus,
        });
      }

      return {
        content: contentBlocks,
        structuredContent: {
          filesLoaded: contentBlocks.filter((b) => b.type === "resource").length,
          projectsFound: discoveryResult.strictMatches.length,
          projectDisconnects: discoveryResult.looseMatches.length,
          projectSuggestions: discoveryResult.suggestions.length,
        },
      };
    }
  );
}
