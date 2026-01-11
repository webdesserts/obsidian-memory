/**
 * Obsidian Filesystem bridge for WASM.
 *
 * Provides callback functions that the Rust WASM code can use to access
 * the Obsidian vault through the Vault API.
 */

import { Vault } from "obsidian";
import { JsFileSystemBridge } from "../wasm";

export interface FileStat {
  mtime: number;
  size: number;
  isDir: boolean;
}

export interface FileEntry {
  name: string;
  isDir: boolean;
}

/**
 * Creates a JsFileSystemBridge that wraps Obsidian's Vault API.
 *
 * The bridge provides async callbacks that the Rust WASM code uses
 * to read/write files through Obsidian's adapter.
 */
export function createFsBridge(vault: Vault): JsFileSystemBridge {
  const read = async (path: string): Promise<Uint8Array> => {
    const content = await vault.adapter.readBinary(path);
    return new Uint8Array(content);
  };

  const write = async (path: string, data: Uint8Array): Promise<void> => {
    // Ensure parent directory exists before writing
    const lastSlash = path.lastIndexOf("/");
    if (lastSlash > 0) {
      const parentDir = path.substring(0, lastSlash);
      if (!(await vault.adapter.exists(parentDir))) {
        // Recursively create parent directories
        await mkdirRecursive(parentDir);
      }
    }
    
    // Need to convert to ArrayBuffer (not SharedArrayBuffer) for Obsidian's API
    await vault.adapter.writeBinary(path, data.buffer as ArrayBuffer);
  };
  
  /** Recursively create directory and all parents */
  const mkdirRecursive = async (path: string): Promise<void> => {
    if (await vault.adapter.exists(path)) {
      return;
    }
    
    // Create parent first
    const lastSlash = path.lastIndexOf("/");
    if (lastSlash > 0) {
      const parentDir = path.substring(0, lastSlash);
      await mkdirRecursive(parentDir);
    }
    
    await vault.adapter.mkdir(path);
  };

  const list = async (path: string): Promise<FileEntry[]> => {
    const files = await vault.adapter.list(path);
    const entries: FileEntry[] = [];

    for (const file of files.files) {
      const name = file.split("/").pop() || file;
      entries.push({ name, isDir: false });
    }

    for (const folder of files.folders) {
      const name = folder.split("/").pop() || folder;
      entries.push({ name, isDir: true });
    }

    return entries;
  };

  const del = async (path: string): Promise<void> => {
    await vault.adapter.remove(path);
  };

  const exists = async (path: string): Promise<boolean> => {
    return await vault.adapter.exists(path);
  };

  const stat = async (path: string): Promise<FileStat> => {
    const s = await vault.adapter.stat(path);
    if (!s) {
      throw new Error(`File not found: ${path}`);
    }
    return {
      mtime: s.mtime,
      size: s.size,
      isDir: s.type === "folder",
    };
  };

  const mkdir = async (path: string): Promise<void> => {
    await vault.adapter.mkdir(path);
  };

  return new JsFileSystemBridge(read, write, list, del, exists, stat, mkdir);
}
