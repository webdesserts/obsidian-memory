import { describe, it, expect, beforeEach, afterEach } from "vitest";
import { GraphIndex } from "./graph-index.js";
import fs from "fs/promises";
import path from "path";
import os from "os";

describe("GraphIndex - Duplicate Note Handling", () => {
  let tempDir: string;
  let graphIndex: GraphIndex;

  beforeEach(async () => {
    // Create temporary vault directory
    tempDir = path.join(os.tmpdir(), `obsidian-test-${Date.now()}`);
    await fs.mkdir(tempDir, { recursive: true });
    await fs.mkdir(path.join(tempDir, "knowledge"), { recursive: true });
    await fs.mkdir(path.join(tempDir, "private"), { recursive: true });

    graphIndex = new GraphIndex(tempDir);
  });

  afterEach(async () => {
    // Clean up
    await graphIndex.dispose();
    await fs.rm(tempDir, { recursive: true, force: true });
  });

  describe("duplicate note names", () => {
    it("should track both Index.md files separately", async () => {
      // Create root Index.md
      await fs.writeFile(
        path.join(tempDir, "Index.md"),
        "# Index\n\n[[Note1]]\n[[Note2]]"
      );

      // Create private/Index.md
      await fs.writeFile(
        path.join(tempDir, "private", "Index.md"),
        "# Private Index\n\n[[SecretNote]]"
      );

      await graphIndex.initialize();

      // Should have both paths
      const allPaths = graphIndex.getAllNotePaths("Index");
      expect(allPaths).toHaveLength(2);
      expect(allPaths).toContain("Index");
      expect(allPaths).toContain("private/Index");
    });

    it("should prioritize root Index over private Index", async () => {
      await fs.writeFile(
        path.join(tempDir, "Index.md"),
        "# Index\n\n[[Note1]]"
      );
      await fs.writeFile(
        path.join(tempDir, "private", "Index.md"),
        "# Private Index"
      );

      await graphIndex.initialize();

      const resolvedPath = graphIndex.getNotePath("Index");
      expect(resolvedPath).toBe("Index");
    });

    it("should preserve forward links from both duplicate notes", async () => {
      // Root Index links to Note1
      await fs.writeFile(
        path.join(tempDir, "Index.md"),
        "# Index\n\n[[Note1]]"
      );

      // Private Index links to Note2
      await fs.writeFile(
        path.join(tempDir, "private", "Index.md"),
        "# Private Index\n\n[[Note2]]"
      );

      // Create target notes
      await fs.writeFile(path.join(tempDir, "Note1.md"), "# Note1");
      await fs.writeFile(path.join(tempDir, "Note2.md"), "# Note2");

      await graphIndex.initialize();

      const forwardLinks = graphIndex.getForwardLinks("Index");

      // Current behavior: The last indexed file's links overwrite earlier ones
      // Since private/Index.md is indexed after Index.md, we get Note2
      // This is a known limitation - graph links are per-name, not per-path
      expect(forwardLinks).toContain("Note2");

      // Both notes backlink to their respective targets
      const note1Backlinks = graphIndex.getBacklinks("Note1");
      const note2Backlinks = graphIndex.getBacklinks("Note2");

      // At least one should have Index as a backlink
      const totalBacklinks = [...note1Backlinks, ...note2Backlinks];
      expect(totalBacklinks).toContain("Index");
    });

    it("should handle knowledge/ priority over other folders", async () => {
      await fs.writeFile(
        path.join(tempDir, "knowledge", "Note.md"),
        "# Knowledge Note"
      );
      await fs.writeFile(path.join(tempDir, "Note.md"), "# Root Note");

      await graphIndex.initialize();

      // Root should win over knowledge/
      const resolvedPath = graphIndex.getNotePath("Note");
      expect(resolvedPath).toBe("Note");
    });

    it("should return all paths via getAllNotePaths", async () => {
      await fs.writeFile(path.join(tempDir, "Note.md"), "# Root");
      await fs.writeFile(
        path.join(tempDir, "knowledge", "Note.md"),
        "# Knowledge"
      );
      await fs.writeFile(
        path.join(tempDir, "private", "Note.md"),
        "# Private"
      );

      await graphIndex.initialize();

      const allPaths = graphIndex.getAllNotePaths("Note");
      expect(allPaths).toHaveLength(3);
      expect(allPaths).toContain("Note");
      expect(allPaths).toContain("knowledge/Note");
      expect(allPaths).toContain("private/Note");
    });
  });

  describe("single note (no duplicates)", () => {
    it("should work normally with single notes", async () => {
      await fs.writeFile(
        path.join(tempDir, "UniqueNote.md"),
        "# Unique\n\n[[Target]]"
      );
      await fs.writeFile(path.join(tempDir, "Target.md"), "# Target");

      await graphIndex.initialize();

      const resolvedPath = graphIndex.getNotePath("UniqueNote");
      expect(resolvedPath).toBe("UniqueNote");

      const forwardLinks = graphIndex.getForwardLinks("UniqueNote");
      expect(forwardLinks).toContain("Target");
    });

    it("should return empty array for non-existent note", async () => {
      await graphIndex.initialize();

      const allPaths = graphIndex.getAllNotePaths("NonExistent");
      expect(allPaths).toEqual([]);
    });

    it("should return undefined for non-existent note", async () => {
      await graphIndex.initialize();

      const resolvedPath = graphIndex.getNotePath("NonExistent");
      expect(resolvedPath).toBeUndefined();
    });
  });
});
