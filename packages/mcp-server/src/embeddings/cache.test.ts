import { describe, it, expect, beforeEach, afterEach } from "vitest";
import { EmbeddingCache } from "./cache.js";
import fs from "fs/promises";
import path from "path";
import os from "os";

describe("EmbeddingCache", () => {
  let tempDir: string;
  let cache: EmbeddingCache;

  beforeEach(async () => {
    // Create temporary vault directory
    tempDir = path.join(os.tmpdir(), `cache-test-${Date.now()}`);
    await fs.mkdir(tempDir, { recursive: true });
    await fs.mkdir(path.join(tempDir, ".obsidian"), { recursive: true });

    cache = new EmbeddingCache(tempDir);
  });

  afterEach(async () => {
    // Clean up
    await fs.rm(tempDir, { recursive: true, force: true });
  });

  describe("persistence", () => {
    it("should persist and restore from disk", async () => {
      const embedding = new Float32Array([1.0, 2.0, 3.0]);
      const content = "Test content";

      // Set and save
      cache.set("test", content, embedding);
      await cache.save();

      // Create new cache instance and load
      const cache2 = new EmbeddingCache(tempDir);
      await cache2.load();

      // Should restore the embedding
      const restored = await cache2.get("test", content);
      expect(restored).toEqual(embedding);
    });

    it("should persist multiple embeddings", async () => {
      const embeddings = {
        note1: new Float32Array([1.0, 2.0, 3.0]),
        note2: new Float32Array([4.0, 5.0, 6.0]),
        note3: new Float32Array([7.0, 8.0, 9.0]),
      };

      // Set all embeddings
      cache.set("note1", "Content 1", embeddings.note1);
      cache.set("note2", "Content 2", embeddings.note2);
      cache.set("note3", "Content 3", embeddings.note3);

      await cache.save();

      // Load in new cache
      const cache2 = new EmbeddingCache(tempDir);
      await cache2.load();

      // All embeddings should be restored
      expect(await cache2.get("note1", "Content 1")).toEqual(embeddings.note1);
      expect(await cache2.get("note2", "Content 2")).toEqual(embeddings.note2);
      expect(await cache2.get("note3", "Content 3")).toEqual(embeddings.note3);
    });

    it("should handle missing cache file gracefully", async () => {
      // Load without cache file - should not error
      await cache.load();

      // Cache should be empty
      const stats = cache.getStats();
      expect(stats.size).toBe(0);
    });

    it("should handle corrupted cache file gracefully", async () => {
      // Write invalid JSON
      const cacheFilePath = path.join(tempDir, ".obsidian", "embedding-cache.json");
      await fs.writeFile(cacheFilePath, "{ invalid json }");

      // Load should not error, just start fresh
      await cache.load();

      const stats = cache.getStats();
      expect(stats.size).toBe(0);
    });
  });

  describe("content-based invalidation", () => {
    it("should return null when content changes", async () => {
      const embedding = new Float32Array([1.0, 2.0, 3.0]);
      const originalContent = "Original content";
      const modifiedContent = "Modified content";

      // Cache with original content
      cache.set("test", originalContent, embedding);

      // Get with original content - cache hit
      const hit = await cache.get("test", originalContent);
      expect(hit).toEqual(embedding);

      // Get with modified content - cache miss
      const miss = await cache.get("test", modifiedContent);
      expect(miss).toBeNull();
    });

    it("should detect even small content changes", async () => {
      const embedding = new Float32Array([1.0, 2.0, 3.0]);
      const content1 = "Hello world";
      const content2 = "Hello world!"; // Added exclamation

      cache.set("test", content1, embedding);

      const hit = await cache.get("test", content1);
      expect(hit).toEqual(embedding);

      const miss = await cache.get("test", content2);
      expect(miss).toBeNull();
    });
  });

  describe("model version mismatch", () => {
    it("should reject cache with different model", async () => {
      // Create cache with one model
      const cache1 = new EmbeddingCache(tempDir, "model-v1");
      cache1.set("test", "content", new Float32Array([1, 2, 3]));
      await cache1.save();

      // Load with different model
      const cache2 = new EmbeddingCache(tempDir, "model-v2");
      await cache2.load();

      // Should start fresh (empty cache)
      const stats = cache2.getStats();
      expect(stats.size).toBe(0);
      expect(stats.model).toBe("model-v2");
    });

    it("should accept cache with same model", async () => {
      const modelName = "all-MiniLM-L6-v2";
      const embedding = new Float32Array([1, 2, 3]);

      // Create cache with model
      const cache1 = new EmbeddingCache(tempDir, modelName);
      cache1.set("test", "content", embedding);
      await cache1.save();

      // Load with same model
      const cache2 = new EmbeddingCache(tempDir, modelName);
      await cache2.load();

      // Should restore embeddings
      const restored = await cache2.get("test", "content");
      expect(restored).toEqual(embedding);
    });
  });

  describe("cache operations", () => {
    it("should invalidate specific entries", () => {
      cache.set("note1", "Content 1", new Float32Array([1, 2, 3]));
      cache.set("note2", "Content 2", new Float32Array([4, 5, 6]));

      expect(cache.has("note1")).toBe(true);
      expect(cache.has("note2")).toBe(true);

      // Invalidate note1
      cache.invalidate("note1");

      expect(cache.has("note1")).toBe(false);
      expect(cache.has("note2")).toBe(true);
    });

    it("should clear all entries", () => {
      cache.set("note1", "Content 1", new Float32Array([1, 2, 3]));
      cache.set("note2", "Content 2", new Float32Array([4, 5, 6]));

      expect(cache.getStats().size).toBe(2);

      cache.clear();

      expect(cache.getStats().size).toBe(0);
    });

    it("should update existing entries", async () => {
      const embedding1 = new Float32Array([1, 2, 3]);
      const embedding2 = new Float32Array([4, 5, 6]);

      // Set initial embedding
      cache.set("test", "content", embedding1);

      // Update with new embedding
      cache.set("test", "content", embedding2);

      // Should have the new embedding
      const result = await cache.get("test", "content");
      expect(result).toEqual(embedding2);
    });
  });

  describe("statistics", () => {
    it("should track cache size", () => {
      expect(cache.getStats().size).toBe(0);

      cache.set("note1", "Content 1", new Float32Array([1, 2, 3]));
      expect(cache.getStats().size).toBe(1);

      cache.set("note2", "Content 2", new Float32Array([4, 5, 6]));
      expect(cache.getStats().size).toBe(2);

      cache.invalidate("note1");
      expect(cache.getStats().size).toBe(1);
    });

    it("should include metadata in stats", () => {
      const stats = cache.getStats();

      expect(stats.model).toBe("all-MiniLM-L6-v2"); // Default model
      expect(stats.created).toBeDefined();
      expect(stats.lastUpdated).toBeDefined();
    });
  });

  describe("hash collision resistance", () => {
    // SHA-256 should prevent collisions even for similar content
    it("should distinguish similar but different content", async () => {
      const embedding1 = new Float32Array([1, 2, 3]);
      const embedding2 = new Float32Array([4, 5, 6]);

      // Set with different content (different capitalization)
      cache.set("note1", "Hello world", embedding1);
      cache.set("note2", "Hello World", embedding2);

      // Content with different case should not match
      const result1 = await cache.get("note1", "Hello world");
      const result2 = await cache.get("note2", "Hello World");

      expect(result1).toEqual(embedding1);
      expect(result2).toEqual(embedding2);
      expect(result1).not.toEqual(result2);
    });
  });
});
