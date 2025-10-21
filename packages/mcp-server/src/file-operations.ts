import fs from "fs/promises";
import matter from "gray-matter";
import { validatePath, fileExists, ensureMarkdownExtension } from "@webdesserts/obsidian-memory-utils";

export interface FileOperationsConfig {
  vaultPath: string;
}

export class FileOperations {
  constructor(private config: FileOperationsConfig) {}

  /**
   * Read note content with optional frontmatter parsing
   */
  async readNote(relativePath: string): Promise<{
    content: string;
    rawContent: string;
    frontmatter?: any;
  }> {
    const notePath = ensureMarkdownExtension(relativePath);
    const absolutePath = validatePath(this.config.vaultPath, notePath);

    if (!(await fileExists(absolutePath))) {
      throw new Error(`Note not found: ${relativePath}`);
    }

    const fileContent = await fs.readFile(absolutePath, "utf-8");
    const parsed = matter(fileContent);

    return {
      content: parsed.content,
      rawContent: fileContent,
      frontmatter: Object.keys(parsed.data).length > 0 ? parsed.data : undefined,
    };
  }

  /**
   * Write note content with optional frontmatter
   */
  async writeNote(
    relativePath: string,
    content: string,
    options: {
      mode?: "overwrite" | "append" | "prepend";
      frontmatter?: Record<string, any>;
    } = {}
  ): Promise<void> {
    const { mode = "overwrite", frontmatter } = options;
    const notePath = ensureMarkdownExtension(relativePath);
    const absolutePath = validatePath(this.config.vaultPath, notePath);

    let finalContent = content;
    let finalFrontmatter = frontmatter;

    // Handle append/prepend modes
    if (mode !== "overwrite" && (await fileExists(absolutePath))) {
      const existing = await this.readNote(relativePath);

      // Merge frontmatter
      if (existing.frontmatter) {
        finalFrontmatter = { ...existing.frontmatter, ...frontmatter };
      }

      // Merge content
      if (mode === "append") {
        finalContent = existing.content + "\n" + content;
      } else if (mode === "prepend") {
        finalContent = content + "\n" + existing.content;
      }
    }

    // Build final file content with frontmatter
    let fileContent = finalContent;
    if (finalFrontmatter && Object.keys(finalFrontmatter).length > 0) {
      fileContent = matter.stringify(finalContent, finalFrontmatter);
    }

    // Ensure parent directory exists
    const parentDir = absolutePath.substring(0, absolutePath.lastIndexOf("/"));
    await fs.mkdir(parentDir, { recursive: true });

    await fs.writeFile(absolutePath, fileContent, "utf-8");
  }

  /**
   * Get frontmatter from a note
   */
  async getFrontmatter(relativePath: string): Promise<Record<string, any> | null> {
    const { frontmatter } = await this.readNote(relativePath);
    return frontmatter || null;
  }

  /**
   * Update frontmatter in a note
   */
  async updateFrontmatter(
    relativePath: string,
    updates: Record<string, any>
  ): Promise<void> {
    const { content, frontmatter } = await this.readNote(relativePath);
    const mergedFrontmatter = { ...frontmatter, ...updates };

    await this.writeNote(relativePath, content, {
      mode: "overwrite",
      frontmatter: mergedFrontmatter,
    });
  }
}
