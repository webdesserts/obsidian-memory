/**
 * Tests for MockVault to verify our test infrastructure works.
 */

import { describe, it, expect, beforeEach } from "vitest";
import { MockVault } from "./mocks/MockVault";
import { TFile } from "./mocks/obsidian";

describe("MockVault", () => {
  let vault: MockVault;

  beforeEach(() => {
    vault = new MockVault("test-vault");
  });

  describe("basic operations", () => {
    it("should create and read files", async () => {
      await vault.create("test.md", "# Hello World");

      const content = vault.getFileContent("test.md");
      expect(content).toBe("# Hello World");
    });

    it("should modify existing files", async () => {
      const file = await vault.create("test.md", "# Original");
      await vault.modify(file, "# Modified");

      const content = vault.getFileContent("test.md");
      expect(content).toBe("# Modified");
    });

    it("should delete files", async () => {
      const file = await vault.create("test.md", "# To Delete");
      await vault.delete(file);

      const exists = await vault.adapter.exists("test.md");
      expect(exists).toBe(false);
    });

    it("should list files in directory", async () => {
      await vault.create("note1.md", "# Note 1");
      await vault.create("note2.md", "# Note 2");

      const { files } = await vault.adapter.list("");
      expect(files).toContain("note1.md");
      expect(files).toContain("note2.md");
    });
  });

  describe("events", () => {
    it("should emit create event when file is created", async () => {
      const events: TFile[] = [];
      vault.on("create", (file: TFile) => events.push(file));

      await vault.create("test.md", "# Content");

      expect(events).toHaveLength(1);
      expect(events[0].path).toBe("test.md");
    });

    it("should emit modify event when file is modified", async () => {
      const events: TFile[] = [];
      vault.on("modify", (file: TFile) => events.push(file));

      const file = await vault.create("test.md", "# Original");
      await vault.modify(file, "# Modified");

      expect(events).toHaveLength(1);
      expect(events[0].path).toBe("test.md");
    });

    it("should emit delete event when file is deleted", async () => {
      const events: TFile[] = [];
      vault.on("delete", (file: TFile) => events.push(file));

      const file = await vault.create("test.md", "# Content");
      await vault.delete(file);

      expect(events).toHaveLength(1);
      expect(events[0].path).toBe("test.md");
    });
  });

  describe("folders", () => {
    it("should create folders", async () => {
      await vault.createFolder("knowledge");

      const exists = await vault.adapter.exists("knowledge");
      expect(exists).toBe(true);
    });

    it("should list folders", async () => {
      await vault.createFolder("knowledge");
      await vault.create("knowledge/note.md", "# Note");

      const { folders } = await vault.adapter.list("");
      expect(folders).toContain("knowledge");
    });
  });

  describe("getAbstractFileByPath", () => {
    it("should return TFile for existing file", async () => {
      await vault.create("test.md", "# Content");

      const file = vault.getAbstractFileByPath("test.md");
      expect(file).toBeInstanceOf(TFile);
      expect(file?.path).toBe("test.md");
    });

    it("should return null for non-existent file", () => {
      const file = vault.getAbstractFileByPath("nonexistent.md");
      expect(file).toBeNull();
    });
  });
});
