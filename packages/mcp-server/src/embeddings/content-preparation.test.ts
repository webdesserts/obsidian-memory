import { describe, it, expect } from "vitest";
import { prepareContentForEmbedding } from "./manager.js";
import crypto from "crypto";

describe("prepareContentForEmbedding", () => {
  describe("content format", () => {
    it("should prepend note name to content", () => {
      const noteName = "Test Note";
      const content = "This is the content";

      const result = prepareContentForEmbedding(noteName, content);

      expect(result).toBe("Test Note\n\nThis is the content");
    });

    it("should separate title and content with double newline", () => {
      const result = prepareContentForEmbedding("Note", "Content");

      // Should have exactly two newlines between title and content
      expect(result).toContain("\n\n");
      expect(result.split("\n\n")).toHaveLength(2);
    });

    it("should handle empty content", () => {
      const result = prepareContentForEmbedding("Empty Note", "");

      expect(result).toBe("Empty Note\n\n");
    });

    it("should handle multiline content", () => {
      const content = "Line 1\nLine 2\nLine 3";
      const result = prepareContentForEmbedding("Note", content);

      expect(result).toBe("Note\n\nLine 1\nLine 2\nLine 3");
    });

    it("should preserve existing newlines in content", () => {
      const content = "\n\nContent with leading newlines";
      const result = prepareContentForEmbedding("Note", content);

      expect(result).toBe("Note\n\n\n\nContent with leading newlines");
    });
  });

  describe("consistency for caching", () => {
    it("should produce identical output for same inputs", () => {
      const noteName = "Consistent Note";
      const content = "Same content";

      const result1 = prepareContentForEmbedding(noteName, content);
      const result2 = prepareContentForEmbedding(noteName, content);

      expect(result1).toBe(result2);
    });

    it("should produce same hash for same inputs", () => {
      const noteName = "Hash Test";
      const content = "Content for hashing";

      const prepared1 = prepareContentForEmbedding(noteName, content);
      const prepared2 = prepareContentForEmbedding(noteName, content);

      const hash1 = crypto.createHash("sha256").update(prepared1).digest("hex");
      const hash2 = crypto.createHash("sha256").update(prepared2).digest("hex");

      expect(hash1).toBe(hash2);
    });

    it("should produce different output for different note names", () => {
      const content = "Same content";

      const result1 = prepareContentForEmbedding("Note A", content);
      const result2 = prepareContentForEmbedding("Note B", content);

      expect(result1).not.toBe(result2);
    });

    it("should produce different output for different content", () => {
      const noteName = "Same Note";

      const result1 = prepareContentForEmbedding(noteName, "Content A");
      const result2 = prepareContentForEmbedding(noteName, "Content B");

      expect(result1).not.toBe(result2);
    });
  });

  describe("edge cases", () => {
    it("should handle special characters in note name", () => {
      const noteName = "Note with / and \\ and @#$";
      const content = "Content";

      const result = prepareContentForEmbedding(noteName, content);

      expect(result).toContain(noteName);
      expect(result).toContain(content);
    });

    it("should handle unicode in note name", () => {
      const noteName = "Note with 中文 and émojis 🎉";
      const content = "Content";

      const result = prepareContentForEmbedding(noteName, content);

      expect(result).toBe("Note with 中文 and émojis 🎉\n\nContent");
    });

    it("should handle very long note names", () => {
      const noteName = "A".repeat(1000);
      const content = "Content";

      const result = prepareContentForEmbedding(noteName, content);

      expect(result.startsWith(noteName)).toBe(true);
      expect(result.endsWith(content)).toBe(true);
    });

    it("should handle very long content", () => {
      const noteName = "Note";
      const content = "B".repeat(10000);

      const result = prepareContentForEmbedding(noteName, content);

      expect(result.startsWith(noteName)).toBe(true);
      expect(result.endsWith(content)).toBe(true);
    });
  });

  describe("cache invalidation scenarios", () => {
    // These tests verify that changes result in different hashes (cache miss)

    it("should invalidate cache when note is renamed", () => {
      const content = "Same content";

      const prepared1 = prepareContentForEmbedding("Old Name", content);
      const prepared2 = prepareContentForEmbedding("New Name", content);

      const hash1 = crypto.createHash("sha256").update(prepared1).digest("hex");
      const hash2 = crypto.createHash("sha256").update(prepared2).digest("hex");

      // Different hashes mean cache miss (correct behavior)
      expect(hash1).not.toBe(hash2);
    });

    it("should invalidate cache when content changes", () => {
      const noteName = "Note";

      const prepared1 = prepareContentForEmbedding(noteName, "Old content");
      const prepared2 = prepareContentForEmbedding(noteName, "New content");

      const hash1 = crypto.createHash("sha256").update(prepared1).digest("hex");
      const hash2 = crypto.createHash("sha256").update(prepared2).digest("hex");

      // Different hashes mean cache miss (correct behavior)
      expect(hash1).not.toBe(hash2);
    });

    it("should invalidate cache when whitespace changes", () => {
      const noteName = "Note";

      const prepared1 = prepareContentForEmbedding(noteName, "Content");
      const prepared2 = prepareContentForEmbedding(noteName, "Content ");

      const hash1 = crypto.createHash("sha256").update(prepared1).digest("hex");
      const hash2 = crypto.createHash("sha256").update(prepared2).digest("hex");

      // Different hashes mean cache miss (correct behavior)
      expect(hash1).not.toBe(hash2);
    });
  });
});
