import fs from "fs/promises";
import path from "path";
import crypto from "crypto";
import type {
  CachedEmbedding,
  EmbeddingCacheData,
  EmbeddingCacheMetadata,
  EmbeddingVector,
} from "./types.js";

/**
 * Cache for computed embeddings with content-based invalidation
 *
 * Uses SHA-256 hashing to detect file changes and invalidate stale embeddings.
 * Persists cache to disk for faster startup on subsequent runs.
 */
export class EmbeddingCache {
  private cache = new Map<string, CachedEmbedding>();
  private cacheFilePath: string;
  private modelName: string;
  private metadata: EmbeddingCacheMetadata;

  constructor(vaultPath: string, modelName = "all-MiniLM-L6-v2") {
    this.cacheFilePath = path.join(
      vaultPath,
      ".obsidian",
      "embedding-cache.json"
    );
    this.modelName = modelName;
    this.metadata = {
      model: modelName,
      created: new Date().toISOString(),
      lastUpdated: new Date().toISOString(),
      count: 0,
    };
  }

  /**
   * Load cache from disk if it exists
   */
  async load(): Promise<void> {
    try {
      const data = await fs.readFile(this.cacheFilePath, "utf-8");
      const parsed: EmbeddingCacheData = JSON.parse(data);

      // Validate model matches
      if (parsed.metadata.model !== this.modelName) {
        console.warn(
          `[EmbeddingCache] Model mismatch: cache has ${parsed.metadata.model}, expected ${this.modelName}. Starting fresh.`
        );
        return;
      }

      // Load embeddings into memory
      for (const [filePath, entry] of Object.entries(parsed.embeddings)) {
        this.cache.set(filePath, entry);
      }

      this.metadata = parsed.metadata;

      console.error(
        `[EmbeddingCache] Loaded ${this.cache.size} cached embeddings`
      );
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code === "ENOENT") {
        console.error(
          "[EmbeddingCache] No cache file found, starting fresh"
        );
      } else {
        console.error(
          `[EmbeddingCache] Failed to load cache: ${error}. Starting fresh.`
        );
      }
    }
  }

  /**
   * Persist cache to disk
   */
  async save(): Promise<void> {
    try {
      // Ensure .obsidian directory exists
      const dir = path.dirname(this.cacheFilePath);
      await fs.mkdir(dir, { recursive: true });

      // Convert Map to Record for JSON serialization
      const embeddings: Record<string, CachedEmbedding> = {};
      for (const [filePath, entry] of this.cache.entries()) {
        embeddings[filePath] = entry;
      }

      const data: EmbeddingCacheData = {
        metadata: {
          ...this.metadata,
          lastUpdated: new Date().toISOString(),
          count: this.cache.size,
        },
        embeddings,
      };

      await fs.writeFile(this.cacheFilePath, JSON.stringify(data, null, 2));

      console.error(
        `[EmbeddingCache] Saved ${this.cache.size} embeddings to disk`
      );
    } catch (error) {
      console.error(`[EmbeddingCache] Failed to save cache: ${error}`);
    }
  }

  /**
   * Compute SHA-256 hash of file content
   */
  private hashContent(content: string): string {
    return crypto.createHash("sha256").update(content, "utf8").digest("hex");
  }

  /**
   * Get cached embedding for a file if it exists and is valid
   *
   * Returns null if cache miss or content hash mismatch
   */
  async get(
    filePath: string,
    currentContent: string
  ): Promise<EmbeddingVector | null> {
    const cached = this.cache.get(filePath);
    if (!cached) {
      return null;
    }

    // Check if content has changed
    const currentHash = this.hashContent(currentContent);
    if (cached.contentHash !== currentHash) {
      // Content changed, invalidate cache entry
      this.cache.delete(filePath);
      return null;
    }

    // Cache hit - convert number array back to Float32Array
    return new Float32Array(cached.embedding);
  }

  /**
   * Store embedding in cache
   */
  set(
    filePath: string,
    content: string,
    embedding: EmbeddingVector
  ): void {
    const contentHash = this.hashContent(content);

    this.cache.set(filePath, {
      contentHash,
      embedding: Array.from(embedding), // Convert Float32Array to number[] for JSON
      timestamp: new Date().toISOString(),
      path: filePath,
    });
  }

  /**
   * Invalidate cache entry for a file
   */
  invalidate(filePath: string): void {
    this.cache.delete(filePath);
  }

  /**
   * Clear all cached embeddings
   */
  clear(): void {
    this.cache.clear();
  }

  /**
   * Get cache statistics
   */
  getStats(): {
    size: number;
    model: string;
    created: string;
    lastUpdated: string;
  } {
    return {
      size: this.cache.size,
      model: this.metadata.model,
      created: this.metadata.created,
      lastUpdated: this.metadata.lastUpdated,
    };
  }

  /**
   * Check if a file is cached (regardless of content hash)
   */
  has(filePath: string): boolean {
    return this.cache.has(filePath);
  }
}
