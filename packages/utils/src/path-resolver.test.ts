import { describe, it, expect } from "vitest";
import {
  resolveNotePath,
  generateSearchPaths,
  normalizeNoteReference,
  extractNoteName,
} from "./path-resolver.js";

describe("resolveNotePath()", () => {
  it("should return undefined for empty paths array", () => {
    const result = resolveNotePath([]);
    expect(result).toBeUndefined();
  });

  it("should return the only path when there's one option", () => {
    const result = resolveNotePath(["knowledge/Test"]);
    expect(result).toBe("knowledge/Test");
  });

  it("should prioritize root-level notes", () => {
    const paths = ["private/Index", "Index", "knowledge/Index"];
    const result = resolveNotePath(paths);
    expect(result).toBe("Index");
  });

  it("should prioritize knowledge/ over journal/", () => {
    const paths = ["journal/Note", "knowledge/Note", "private/Note"];
    const result = resolveNotePath(paths);
    expect(result).toBe("knowledge/Note");
  });

  it("should prioritize journal/ over other folders", () => {
    const paths = ["private/Note", "other/Note", "journal/Note"];
    const result = resolveNotePath(paths);
    expect(result).toBe("journal/Note");
  });

  it("should deprioritize private/ by default", () => {
    const paths = ["private/Note", "other/Note"];
    const result = resolveNotePath(paths);
    expect(result).toBe("other/Note");
  });

  it("should include private/ when includePrivate is true", () => {
    const paths = ["private/Note"];
    const result = resolveNotePath(paths, { includePrivate: true });
    expect(result).toBe("private/Note");
  });

  it("should filter out private/ when includePrivate is false and alternatives exist", () => {
    const paths = ["private/Note", "Note"];
    const result = resolveNotePath(paths, { includePrivate: false });
    expect(result).toBe("Note");
  });

  it("should return private/ as fallback if it's the only option", () => {
    const paths = ["private/Note"];
    const result = resolveNotePath(paths, { includePrivate: false });
    expect(result).toBe("private/Note");
  });
});

describe("generateSearchPaths()", () => {
  it("should generate common search paths", () => {
    const paths = generateSearchPaths("Test");
    expect(paths).toEqual(["Test", "knowledge/Test", "journal/Test"]);
  });

  it("should include private path when requested", () => {
    const paths = generateSearchPaths("Test", true);
    expect(paths).toEqual([
      "Test",
      "knowledge/Test",
      "journal/Test",
      "private/Test",
    ]);
  });

  it("should not include private path by default", () => {
    const paths = generateSearchPaths("Test", false);
    expect(paths).not.toContain("private/Test");
  });
});

describe("normalizeNoteReference()", () => {
  it("should strip memory:// prefix", () => {
    const result = normalizeNoteReference("memory://knowledge/Note");
    expect(result).toBe("knowledge/Note");
  });

  it("should strip .md extension", () => {
    const result = normalizeNoteReference("knowledge/Note.md");
    expect(result).toBe("knowledge/Note");
  });

  it("should strip both prefix and extension", () => {
    const result = normalizeNoteReference("memory://knowledge/Note.md");
    expect(result).toBe("knowledge/Note");
  });

  it("should return note name as-is if already normalized", () => {
    const result = normalizeNoteReference("knowledge/Note");
    expect(result).toBe("knowledge/Note");
  });
});

describe("extractNoteName()", () => {
  it("should extract note name from path", () => {
    const result = extractNoteName("knowledge/subfolder/Note");
    expect(result).toBe("Note");
  });

  it("should extract note name from root-level path", () => {
    const result = extractNoteName("Note");
    expect(result).toBe("Note");
  });

  it("should handle memory:// URIs", () => {
    const result = extractNoteName("memory://knowledge/Note");
    expect(result).toBe("Note");
  });

  it("should handle .md extension", () => {
    const result = extractNoteName("knowledge/Note.md");
    expect(result).toBe("Note");
  });
});
