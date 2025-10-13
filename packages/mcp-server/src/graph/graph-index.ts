import fs from "fs/promises";
import path from "path";
import chokidar, { FSWatcher } from "chokidar";
import { parseWikiLinks, extractLinkedNotes } from "@obsidian-memory/utils";

/**
 * In-memory graph index tracking forward links and backlinks
 */
export class GraphIndex {
  // Map of note name -> list of notes it links to
  private forwardLinks = new Map<string, Set<string>>();

  // Map of note name -> list of notes that link to it
  private backlinks = new Map<string, Set<string>>();

  // Map of note name -> array of relative file paths (handles duplicate note names)
  private notePaths = new Map<string, string[]>();

  // File watcher
  private watcher?: FSWatcher;

  constructor(private vaultPath: string) {}

  /**
   * Initialize the graph index by scanning the vault
   */
  async initialize(): Promise<void> {
    console.error("[GraphIndex] Scanning vault...");

    await this.scanVault();

    console.error(
      `[GraphIndex] Indexed ${this.forwardLinks.size} notes with ${this.getTotalLinks()} links`
    );

    // Set up file watcher for incremental updates
    this.setupFileWatcher();
  }

  /**
   * Scan entire vault and build graph
   */
  private async scanVault(): Promise<void> {
    const files = await this.getAllMarkdownFiles(this.vaultPath);

    for (const filePath of files) {
      await this.indexFile(filePath);
    }
  }

  /**
   * Get all markdown files in vault recursively
   */
  private async getAllMarkdownFiles(dir: string): Promise<string[]> {
    const files: string[] = [];

    const entries = await fs.readdir(dir, { withFileTypes: true });

    for (const entry of entries) {
      const fullPath = path.join(dir, entry.name);

      // Skip .obsidian directory
      if (entry.name === ".obsidian") {
        continue;
      }

      if (entry.isDirectory()) {
        files.push(...(await this.getAllMarkdownFiles(fullPath)));
      } else if (entry.isFile() && entry.name.endsWith(".md")) {
        files.push(fullPath);
      }
    }

    return files;
  }

  /**
   * Index a single file (extract links and update graph)
   */
  private async indexFile(filePath: string): Promise<void> {
    const noteName = this.getNoteName(filePath);
    const relativePath = path.relative(this.vaultPath, filePath);
    const content = await fs.readFile(filePath, "utf-8");

    const linkedNotes = extractLinkedNotes(content);

    // Store note path (for ResourceLinks) - append to array to handle duplicates
    const pathWithoutExt = relativePath.replace(/\.md$/, "");
    const existingPaths = this.notePaths.get(noteName) || [];
    if (!existingPaths.includes(pathWithoutExt)) {
      this.notePaths.set(noteName, [...existingPaths, pathWithoutExt]);
    }

    // Clear existing forward links for this note
    const oldLinks = this.forwardLinks.get(noteName);
    if (oldLinks) {
      // Remove backlinks from previously linked notes
      for (const target of oldLinks) {
        this.backlinks.get(target)?.delete(noteName);
      }
    }

    // Update forward links
    this.forwardLinks.set(noteName, new Set(linkedNotes));

    // Update backlinks
    for (const target of linkedNotes) {
      if (!this.backlinks.has(target)) {
        this.backlinks.set(target, new Set());
      }
      this.backlinks.get(target)!.add(noteName);
    }
  }

  /**
   * Set up file watcher for incremental updates
   */
  private setupFileWatcher(): void {
    this.watcher = chokidar.watch("**/*.md", {
      cwd: this.vaultPath,
      ignored: ".obsidian/**",
      ignoreInitial: true,
      persistent: true,
    });

    this.watcher.on("add", (relativePath: string) => {
      const filePath = path.join(this.vaultPath, relativePath);
      console.error(`[GraphIndex] File added: ${relativePath}`);
      this.indexFile(filePath);
    });

    this.watcher.on("change", (relativePath: string) => {
      const filePath = path.join(this.vaultPath, relativePath);
      console.error(`[GraphIndex] File changed: ${relativePath}`);
      this.indexFile(filePath);
    });

    this.watcher.on("unlink", (relativePath: string) => {
      const noteName = path.basename(relativePath, ".md");
      console.error(`[GraphIndex] File deleted: ${relativePath}`);
      this.removeNote(noteName);
    });
  }

  /**
   * Remove a note from the index
   */
  private removeNote(noteName: string): void {
    // Remove forward links
    const linkedNotes = this.forwardLinks.get(noteName);
    if (linkedNotes) {
      for (const target of linkedNotes) {
        this.backlinks.get(target)?.delete(noteName);
      }
      this.forwardLinks.delete(noteName);
    }

    // Remove backlinks
    this.backlinks.delete(noteName);
  }

  /**
   * Get note name from file path
   */
  private getNoteName(filePath: string): string {
    return path.basename(filePath, ".md");
  }

  /**
   * Get all relative paths for a note (without .md extension)
   * Returns all paths if there are duplicates
   */
  getAllNotePaths(noteName: string): string[] {
    return this.notePaths.get(noteName) || [];
  }

  /**
   * Get relative path for a note (without .md extension)
   * Uses priority order: root → knowledge/ → journal/ → others → private/
   */
  getNotePath(noteName: string): string | undefined {
    const paths = this.notePaths.get(noteName);
    if (!paths || paths.length === 0) return undefined;
    if (paths.length === 1) return paths[0];

    // Priority order for disambiguation
    const priorityOrder = [
      (p: string) => !p.includes("/"), // Root level first
      (p: string) => p.startsWith("knowledge/"),
      (p: string) => p.startsWith("journal/"),
      (p: string) => !p.startsWith("private/"), // Non-private before private
      () => true, // Any remaining (including private)
    ];

    for (const predicate of priorityOrder) {
      const match = paths.find(predicate);
      if (match) return match;
    }

    // Fallback to first path (shouldn't reach here)
    return paths[0];
  }

  /**
   * Get all notes that this note links to
   */
  getForwardLinks(noteName: string): string[] {
    const links = this.forwardLinks.get(noteName);
    return links ? Array.from(links) : [];
  }

  /**
   * Get all notes that link to this note
   */
  getBacklinks(noteName: string, includePrivate: boolean = false): string[] {
    const links = this.backlinks.get(noteName);
    if (!links) return [];

    let filtered = Array.from(links);

    // Filter out private folder links unless explicitly requested
    if (!includePrivate) {
      filtered = filtered.filter((note) => {
        // Check if the linking note is in private folder
        // This is a simplified check - assumes note names are unique
        return !note.startsWith("private/");
      });
    }

    return filtered;
  }

  /**
   * Get graph neighborhood (notes within N hops)
   */
  getNeighborhood(
    noteName: string,
    depth: number = 2,
    includePrivate: boolean = false
  ): Map<
    string,
    {
      distance: number;
      linkType: "forward" | "backward" | "both";
      directLinks: string[];
      backlinks: string[];
    }
  > {
    const neighborhood = new Map();
    const visited = new Set<string>();
    const queue: Array<{ note: string; distance: number }> = [{ note: noteName, distance: 0 }];

    while (queue.length > 0) {
      const { note, distance } = queue.shift()!;

      if (visited.has(note) || distance > depth) {
        continue;
      }

      visited.add(note);

      // Don't include the center note itself
      if (distance === 0) {
        // Add neighbors to queue
        const forward = this.getForwardLinks(note);
        const backward = this.getBacklinks(note, includePrivate);

        for (const linked of forward) {
          queue.push({ note: linked, distance: distance + 1 });
        }

        for (const linking of backward) {
          queue.push({ note: linking, distance: distance + 1 });
        }

        continue;
      }

      // Get forward and backward links
      const forward = this.getForwardLinks(note);
      const backward = this.getBacklinks(note, includePrivate);

      // Determine link type
      let linkType: "forward" | "backward" | "both" = "forward";
      const isLinkedFrom = this.getForwardLinks(noteName).includes(note);
      const isLinkingTo = this.getBacklinks(noteName, includePrivate).includes(note);

      if (isLinkedFrom && isLinkingTo) {
        linkType = "both";
      } else if (isLinkingTo) {
        linkType = "backward";
      }

      neighborhood.set(note, {
        distance,
        linkType,
        directLinks: forward,
        backlinks: backward,
      });

      // Add neighbors to queue
      for (const linked of forward) {
        if (!visited.has(linked)) {
          queue.push({ note: linked, distance: distance + 1 });
        }
      }

      for (const linking of backward) {
        if (!visited.has(linking)) {
          queue.push({ note: linking, distance: distance + 1 });
        }
      }
    }

    return neighborhood;
  }

  /**
   * Get total number of links in graph
   */
  private getTotalLinks(): number {
    let total = 0;
    for (const links of this.forwardLinks.values()) {
      total += links.size;
    }
    return total;
  }

  /**
   * Clean up resources
   */
  async dispose(): Promise<void> {
    if (this.watcher) {
      await this.watcher.close();
    }
  }
}
