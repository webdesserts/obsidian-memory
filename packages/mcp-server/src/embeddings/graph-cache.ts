import fs from "fs/promises";
import path from "path";
import crypto from "crypto";
import type {
  CachedProximity,
  GraphProximityCacheData,
  GraphProximityCacheMetadata,
  ProximityScores,
} from "./types.js";

/**
 * Cache for computed graph proximity scores with link-based invalidation
 *
 * Uses SHA-256 hashing of link signatures to detect graph structure changes.
 * Persists cache to disk for faster startup on subsequent runs.
 */
export class GraphProximityCache {
  private cache = new Map<string, CachedProximity>();
  private cacheFilePath: string;
  private metadata: GraphProximityCacheMetadata;

  constructor(vaultPath: string) {
    this.cacheFilePath = path.join(
      vaultPath,
      ".obsidian",
      "graph-proximity-cache.json"
    );
    this.metadata = {
      algorithm: {
        name: "PersonalizedPageRank",
        restart: 0.15,
        convergence: 1e-6,
        maxIterations: 100,
      },
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
      const parsed: GraphProximityCacheData = JSON.parse(data);

      // Validate algorithm parameters match
      const alg = parsed.metadata.algorithm;
      if (
        alg.name !== this.metadata.algorithm.name ||
        alg.restart !== this.metadata.algorithm.restart ||
        alg.convergence !== this.metadata.algorithm.convergence
      ) {
        console.warn(
          `[GraphProximityCache] Algorithm mismatch. Starting fresh.`
        );
        return;
      }

      // Load proximity scores into memory
      for (const [seedNote, entry] of Object.entries(parsed.proximities)) {
        this.cache.set(seedNote, entry);
      }

      this.metadata = parsed.metadata;

      console.error(
        `[GraphProximityCache] Loaded ${this.cache.size} cached proximity computations`
      );
    } catch (error) {
      if ((error as NodeJS.ErrnoException).code === "ENOENT") {
        console.error(
          "[GraphProximityCache] No cache file found, starting fresh"
        );
      } else {
        console.error(
          `[GraphProximityCache] Failed to load cache: ${error}. Starting fresh.`
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
      const proximities: Record<string, CachedProximity> = {};
      for (const [seedNote, entry] of this.cache.entries()) {
        proximities[seedNote] = entry;
      }

      const data: GraphProximityCacheData = {
        metadata: {
          ...this.metadata,
          lastUpdated: new Date().toISOString(),
          count: this.cache.size,
        },
        proximities,
      };

      await fs.writeFile(this.cacheFilePath, JSON.stringify(data, null, 2));

      console.error(
        `[GraphProximityCache] Saved ${this.cache.size} proximity computations`
      );
    } catch (error) {
      console.error(`[GraphProximityCache] Failed to save cache: ${error}`);
    }
  }

  /**
   * Compute SHA-256 hash of link signature for a note
   *
   * Link signature is: sorted forward links + sorted backlinks
   * Changes to any links invalidate the cache for this seed note
   */
  private computeLinkHash(
    forwardLinks: string[],
    backlinks: string[]
  ): string {
    const signature = `${forwardLinks.sort().join(",")}|${backlinks.sort().join(",")}`;
    return crypto.createHash("sha256").update(signature).digest("hex");
  }

  /**
   * Get cached proximity scores for a seed note
   *
   * Returns null if:
   * - No cache entry exists
   * - Link structure has changed (hash mismatch)
   */
  async get(
    seedNote: string,
    forwardLinks: string[],
    backlinks: string[]
  ): Promise<ProximityScores | null> {
    const cached = this.cache.get(seedNote);
    if (!cached) {
      return null;
    }

    // Verify link structure hasn't changed
    const currentHash = this.computeLinkHash(forwardLinks, backlinks);
    if (cached.linkHash !== currentHash) {
      console.error(
        `[GraphProximityCache] Link structure changed for ${seedNote}, invalidating cache`
      );
      this.cache.delete(seedNote);
      return null;
    }

    // Convert stored Record to Map
    const scores = new Map<string, number>();
    for (const [noteName, score] of Object.entries(cached.scores)) {
      scores.set(noteName, score);
    }

    return scores;
  }

  /**
   * Store computed proximity scores for a seed note
   */
  set(
    seedNote: string,
    forwardLinks: string[],
    backlinks: string[],
    scores: ProximityScores
  ): void {
    const linkHash = this.computeLinkHash(forwardLinks, backlinks);

    // Convert Map to Record for storage
    const scoresRecord: Record<string, number> = {};
    for (const [noteName, score] of scores.entries()) {
      scoresRecord[noteName] = score;
    }

    this.cache.set(seedNote, {
      seedNote,
      linkHash,
      scores: scoresRecord,
      timestamp: new Date().toISOString(),
    });
  }

  /**
   * Invalidate cached proximity for a specific seed note
   */
  invalidate(seedNote: string): void {
    this.cache.delete(seedNote);
  }

  /**
   * Clear entire cache
   */
  clear(): void {
    this.cache.clear();
  }

  /**
   * Get cache statistics
   */
  getStats() {
    return {
      size: this.cache.size,
      metadata: this.metadata,
    };
  }
}
