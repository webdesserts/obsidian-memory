/**
 * Sync scenario tests.
 *
 * These tests validate the high-level sync behavior patterns used in the plugin.
 * Since WASM isn't available in Node tests, we test the TypeScript-level logic
 * and mock the WASM interactions.
 */

import { describe, it, expect, beforeEach, vi } from "vitest";
import { MockVault } from "./mocks/MockVault";
import { TFile } from "./mocks/obsidian";

/**
 * Simulates the version tracking logic used in main.ts for preventing sync loops.
 * This mirrors the lastSyncedVersions Map and version comparison logic.
 */
class SyncLoopPrevention {
  private lastSyncedVersions: Map<string, Uint8Array> = new Map();
  private documentVersions: Map<string, number> = new Map();
  private versionCounter = 0;

  /**
   * Simulate storing a synced version (called after sync writes a file)
   */
  markAsSynced(path: string): void {
    const version = this.getOrCreateVersion(path);
    this.lastSyncedVersions.set(path, this.encodeVersion(version));
  }

  /**
   * Simulate a file modification (increments version)
   */
  onFileModified(path: string): void {
    const current = this.documentVersions.get(path) ?? 0;
    this.documentVersions.set(path, current + 1);
  }

  /**
   * Check if we should skip broadcasting this file change.
   * Returns true if the modification is purely from sync.
   */
  shouldSkipBroadcast(path: string): boolean {
    const lastSynced = this.lastSyncedVersions.get(path);
    if (!lastSynced) {
      return false; // No synced version, this is a local edit
    }

    const currentVersion = this.getOrCreateVersion(path);
    const syncedVersion = this.decodeVersion(lastSynced);

    // If current version equals synced version, skip broadcast
    // (no local edits since sync)
    const shouldSkip = currentVersion === syncedVersion;

    // Clear the synced version after checking
    this.lastSyncedVersions.delete(path);

    return shouldSkip;
  }

  private getOrCreateVersion(path: string): number {
    if (!this.documentVersions.has(path)) {
      this.documentVersions.set(path, ++this.versionCounter);
    }
    return this.documentVersions.get(path)!;
  }

  private encodeVersion(v: number): Uint8Array {
    return new Uint8Array([v]);
  }

  private decodeVersion(encoded: Uint8Array): number {
    return encoded[0];
  }
}

describe("Sync Loop Prevention", () => {
  let prevention: SyncLoopPrevention;

  beforeEach(() => {
    prevention = new SyncLoopPrevention();
  });

  describe("when a file is synced from a peer", () => {
    it("should skip broadcast for the sync-triggered modify event", () => {
      // File synced from peer
      prevention.markAsSynced("note.md");

      // Modify event fires (from the sync write)
      // Should skip broadcast because version matches
      expect(prevention.shouldSkipBroadcast("note.md")).toBe(true);
    });

    it("should broadcast if local edit happens after sync", () => {
      // File synced from peer
      prevention.markAsSynced("note.md");

      // User makes a local edit (version increments)
      prevention.onFileModified("note.md");

      // Should broadcast because version changed
      expect(prevention.shouldSkipBroadcast("note.md")).toBe(false);
    });
  });

  describe("when a file is created locally", () => {
    it("should broadcast new files", () => {
      // New file created locally (no synced version)
      expect(prevention.shouldSkipBroadcast("new.md")).toBe(false);
    });
  });

  describe("when multiple rapid syncs occur", () => {
    it("should handle rapid successive syncs correctly", () => {
      // Rapid syncs from peer
      prevention.markAsSynced("note.md");
      expect(prevention.shouldSkipBroadcast("note.md")).toBe(true);

      prevention.markAsSynced("note.md");
      expect(prevention.shouldSkipBroadcast("note.md")).toBe(true);

      prevention.markAsSynced("note.md");
      expect(prevention.shouldSkipBroadcast("note.md")).toBe(true);
    });

    it("should broadcast after syncs stop and user edits", () => {
      // Multiple syncs
      prevention.markAsSynced("note.md");
      prevention.shouldSkipBroadcast("note.md");

      prevention.markAsSynced("note.md");
      prevention.shouldSkipBroadcast("note.md");

      // Then user edits
      prevention.onFileModified("note.md");
      expect(prevention.shouldSkipBroadcast("note.md")).toBe(false);
    });
  });
});

describe("MockVault Sync Simulation", () => {
  let vaultA: MockVault;
  let vaultB: MockVault;

  beforeEach(() => {
    vaultA = new MockVault("vault-a");
    vaultB = new MockVault("vault-b");
  });

  describe("file creation sync", () => {
    it("should sync new file from A to B", async () => {
      // Create file on vault A
      await vaultA.create("note.md", "# Hello from A");

      // Simulate sync: copy content to vault B
      const content = vaultA.getFileContent("note.md")!;
      await vaultB.setFileContent("note.md", content);

      // Verify B has the file
      expect(vaultB.getFileContent("note.md")).toBe("# Hello from A");
    });

    it("should emit correct events during sync", async () => {
      const createEvents: string[] = [];
      const modifyEvents: string[] = [];

      vaultB.on("create", (file: TFile) => createEvents.push(file.path));
      vaultB.on("modify", (file: TFile) => modifyEvents.push(file.path));

      // Simulate sync of new file to B
      await vaultB.create("note.md", "# Content");

      expect(createEvents).toContain("note.md");
      expect(modifyEvents).not.toContain("note.md"); // create, not modify
    });
  });

  describe("file modification sync", () => {
    it("should sync edits from A to B", async () => {
      // Both vaults have the file
      await vaultA.create("note.md", "# Original");
      await vaultB.create("note.md", "# Original");

      // Edit on A
      const fileA = vaultA.getAbstractFileByPath("note.md") as TFile;
      await vaultA.modify(fileA, "# Modified on A");

      // Sync to B
      const fileB = vaultB.getAbstractFileByPath("note.md") as TFile;
      const newContent = vaultA.getFileContent("note.md")!;
      await vaultB.modify(fileB, newContent);

      expect(vaultB.getFileContent("note.md")).toBe("# Modified on A");
    });

    it("should emit modify event when syncing edits", async () => {
      const modifyEvents: string[] = [];
      vaultB.on("modify", (file: TFile) => modifyEvents.push(file.path));

      // Setup
      await vaultB.create("note.md", "# Original");
      modifyEvents.length = 0; // Clear events from create

      // Sync edit
      const file = vaultB.getAbstractFileByPath("note.md") as TFile;
      await vaultB.modify(file, "# Modified");

      expect(modifyEvents).toContain("note.md");
    });
  });

  describe("bidirectional sync", () => {
    it("should handle both vaults creating different files", async () => {
      // A creates file1
      await vaultA.create("file1.md", "# From A");

      // B creates file2
      await vaultB.create("file2.md", "# From B");

      // Sync: A -> B
      await vaultB.setFileContent("file1.md", vaultA.getFileContent("file1.md")!);

      // Sync: B -> A
      await vaultA.setFileContent("file2.md", vaultB.getFileContent("file2.md")!);

      // Both vaults should have both files
      expect(vaultA.getFileContent("file1.md")).toBe("# From A");
      expect(vaultA.getFileContent("file2.md")).toBe("# From B");
      expect(vaultB.getFileContent("file1.md")).toBe("# From A");
      expect(vaultB.getFileContent("file2.md")).toBe("# From B");
    });
  });
});
