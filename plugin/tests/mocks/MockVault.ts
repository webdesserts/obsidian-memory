/**
 * Mock Obsidian Vault for testing.
 *
 * Provides an in-memory filesystem that mimics Obsidian's Vault API,
 * allowing tests to run without actual file I/O or WASM dependencies.
 */

import { Events, TFile, TFolder, TAbstractFile } from "./obsidian";

export interface FileEntry {
  name: string;
  isDir: boolean;
}

export interface FileStat {
  mtime: number;
  size: number;
  type: "file" | "folder";
}

/**
 * In-memory file storage.
 */
interface StoredFile {
  content: Uint8Array;
  mtime: number;
}

/**
 * Mock DataAdapter that stores files in memory.
 */
export class MockAdapter {
  private files: Map<string, StoredFile> = new Map();
  private folders: Set<string> = new Set();

  constructor() {
    // Root always exists
    this.folders.add("");
  }

  async exists(path: string): Promise<boolean> {
    return this.files.has(path) || this.folders.has(path);
  }

  async read(path: string): Promise<string> {
    const file = this.files.get(path);
    if (!file) {
      throw new Error(`File not found: ${path}`);
    }
    return new TextDecoder().decode(file.content);
  }

  async readBinary(path: string): Promise<ArrayBuffer> {
    const file = this.files.get(path);
    if (!file) {
      throw new Error(`File not found: ${path}`);
    }
    // Create a new ArrayBuffer to avoid SharedArrayBuffer type issues
    const copy = new ArrayBuffer(file.content.byteLength);
    new Uint8Array(copy).set(file.content);
    return copy;
  }

  async write(path: string, content: string): Promise<void> {
    const encoded = new TextEncoder().encode(content);
    this.files.set(path, {
      content: encoded,
      mtime: Date.now(),
    });
  }

  async writeBinary(path: string, content: ArrayBuffer): Promise<void> {
    this.files.set(path, {
      content: new Uint8Array(content),
      mtime: Date.now(),
    });
  }

  async remove(path: string): Promise<void> {
    this.files.delete(path);
    this.folders.delete(path);
  }

  async mkdir(path: string): Promise<void> {
    this.folders.add(path);
  }

  async list(path: string): Promise<{ files: string[]; folders: string[] }> {
    const prefix = path ? `${path}/` : "";
    const files: string[] = [];
    const folders: string[] = [];

    for (const filePath of this.files.keys()) {
      if (filePath.startsWith(prefix)) {
        const rest = filePath.slice(prefix.length);
        // Only include direct children (no nested paths)
        if (!rest.includes("/")) {
          files.push(filePath);
        }
      }
    }

    for (const folderPath of this.folders) {
      if (folderPath.startsWith(prefix) && folderPath !== path) {
        const rest = folderPath.slice(prefix.length);
        if (!rest.includes("/")) {
          folders.push(folderPath);
        }
      }
    }

    return { files, folders };
  }

  async stat(path: string): Promise<FileStat | null> {
    const file = this.files.get(path);
    if (file) {
      return {
        mtime: file.mtime,
        size: file.content.byteLength,
        type: "file",
      };
    }

    if (this.folders.has(path)) {
      return {
        mtime: Date.now(),
        size: 0,
        type: "folder",
      };
    }

    return null;
  }

  /** Test helper: get all file paths */
  getAllFiles(): string[] {
    return Array.from(this.files.keys());
  }

  /** Test helper: get raw content as string */
  getContent(path: string): string | undefined {
    const file = this.files.get(path);
    if (!file) return undefined;
    return new TextDecoder().decode(file.content);
  }

  /** Test helper: set content directly */
  setContent(path: string, content: string): void {
    const encoded = new TextEncoder().encode(content);
    this.files.set(path, {
      content: encoded,
      mtime: Date.now(),
    });
  }
}

/**
 * Mock Obsidian Vault for testing.
 *
 * Emits events like the real Vault when files are created/modified/deleted.
 */
export class MockVault extends Events {
  adapter: MockAdapter;
  private name: string;
  private fileIndex: Map<string, TAbstractFile> = new Map();

  constructor(name: string = "test-vault") {
    super();
    this.name = name;
    this.adapter = new MockAdapter();
  }

  getName(): string {
    return this.name;
  }

  getAbstractFileByPath(path: string): TAbstractFile | null {
    return this.fileIndex.get(path) ?? null;
  }

  async create(path: string, content: string): Promise<TFile> {
    await this.adapter.write(path, content);
    const file = new TFile(path);
    this.fileIndex.set(path, file);
    this.trigger("create", file);
    return file;
  }

  async modify(file: TFile, content: string): Promise<void> {
    await this.adapter.write(file.path, content);
    this.trigger("modify", file);
  }

  async createFolder(path: string): Promise<void> {
    await this.adapter.mkdir(path);
    const folder = new TFolder(path);
    this.fileIndex.set(path, folder);
  }

  async delete(file: TAbstractFile): Promise<void> {
    await this.adapter.remove(file.path);
    this.fileIndex.delete(file.path);
    this.trigger("delete", file);
  }

  async rename(file: TAbstractFile, newPath: string): Promise<void> {
    const oldPath = file.path;
    const content = await this.adapter.read(oldPath);
    await this.adapter.write(newPath, content);
    await this.adapter.remove(oldPath);

    this.fileIndex.delete(oldPath);
    if (file instanceof TFile) {
      const newFile = new TFile(newPath);
      this.fileIndex.set(newPath, newFile);
      this.trigger("rename", newFile, oldPath);
    }
  }

  /** Test helper: simulate external file change (no events) */
  async setFileContent(path: string, content: string): Promise<void> {
    this.adapter.setContent(path, content);
    if (!this.fileIndex.has(path)) {
      this.fileIndex.set(path, new TFile(path));
    }
  }

  /** Test helper: get file content directly */
  getFileContent(path: string): string | undefined {
    return this.adapter.getContent(path);
  }
}
