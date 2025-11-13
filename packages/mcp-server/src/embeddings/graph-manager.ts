import { GraphProximityCache } from "./graph-cache.js";
import type { ProximityScores } from "./types.js";
import type { GraphIndex } from "../graph/graph-index.js";

/**
 * Manager for graph proximity computations using Personalized PageRank
 *
 * Implements random walk algorithm to compute proximity scores from seed notes.
 * Uses singleton pattern for resource efficiency and caching coordination.
 */
export class GraphProximityManager {
  private static instance: GraphProximityManager | null = null;
  private cache: GraphProximityCache;
  private graphIndex: GraphIndex;

  // Algorithm parameters
  private readonly restartProbability = 0.15;
  private readonly convergenceThreshold = 1e-6;
  private readonly maxIterations = 100;

  private constructor(vaultPath: string, graphIndex: GraphIndex) {
    this.cache = new GraphProximityCache(vaultPath);
    this.graphIndex = graphIndex;
  }

  /**
   * Get or create singleton instance
   */
  static async getInstance(
    vaultPath: string,
    graphIndex: GraphIndex
  ): Promise<GraphProximityManager> {
    if (GraphProximityManager.instance) {
      return GraphProximityManager.instance;
    }

    const manager = new GraphProximityManager(vaultPath, graphIndex);
    await manager.initialize();
    GraphProximityManager.instance = manager;
    return manager;
  }

  /**
   * Reset singleton instance (for testing)
   */
  static resetInstance(): void {
    GraphProximityManager.instance = null;
  }

  /**
   * Initialize the graph proximity system
   */
  private async initialize(): Promise<void> {
    console.error("[GraphProximityManager] Initializing...");
    await this.cache.load();
    console.error("[GraphProximityManager] Initialization complete");
  }

  /**
   * Compute graph proximity scores from a seed note using Personalized PageRank
   *
   * Algorithm:
   * 1. Start at seed note
   * 2. At each step: 85% follow random link, 15% restart at seed
   * 3. Count visit frequency for each note
   * 4. Normalize to get proximity scores (0.0-1.0)
   *
   * @param seedNote - Note to compute proximity from
   * @returns Map of note names to proximity scores
   */
  async computeProximity(seedNote: string): Promise<ProximityScores> {
    // Get links for cache key validation
    const forwardLinks = this.graphIndex.getForwardLinks(seedNote);
    const backlinks = this.graphIndex.getBacklinks(seedNote);

    // Check cache first
    const cached = await this.cache.get(seedNote, forwardLinks, backlinks);
    if (cached) {
      return cached;
    }

    console.error(`[GraphProximityManager] Computing proximity from ${seedNote}...`);

    // Get all notes in graph
    const allNotes = this.graphIndex.getAllNotes();
    const noteCount = allNotes.length;

    // Initialize probability distribution (all notes start at 0, seed at 1)
    let probability = new Map<string, number>();
    for (const note of allNotes) {
      probability.set(note, note === seedNote ? 1.0 : 0.0);
    }

    // Run random walk iterations until convergence
    let iteration = 0;
    let converged = false;

    while (iteration < this.maxIterations && !converged) {
      const newProbability = new Map<string, number>();

      // Initialize all notes to 0
      for (const note of allNotes) {
        newProbability.set(note, 0);
      }

      // For each note, distribute its probability to its neighbors
      for (const note of allNotes) {
        const prob = probability.get(note) || 0;
        if (prob === 0) continue;

        // Get outgoing links (both forward links and backlinks - treat graph as bidirectional)
        const neighbors = new Set([
          ...this.graphIndex.getForwardLinks(note),
          ...this.graphIndex.getBacklinks(note),
        ]);

        if (neighbors.size === 0) {
          // Dead end - all probability goes to restart
          newProbability.set(
            seedNote,
            (newProbability.get(seedNote) || 0) + prob
          );
        } else {
          // Distribute probability:
          // - (1 - restart) probability distributed to neighbors
          // - restart probability goes back to seed
          const walkProb = prob * (1 - this.restartProbability);
          const restartProb = prob * this.restartProbability;

          // Distribute walk probability equally among neighbors
          const probPerNeighbor = walkProb / neighbors.size;
          for (const neighbor of neighbors) {
            newProbability.set(
              neighbor,
              (newProbability.get(neighbor) || 0) + probPerNeighbor
            );
          }

          // Add restart probability to seed
          newProbability.set(
            seedNote,
            (newProbability.get(seedNote) || 0) + restartProb
          );
        }
      }

      // Check for convergence (L1 distance < threshold)
      let totalChange = 0;
      for (const note of allNotes) {
        const oldProb = probability.get(note) || 0;
        const newProb = newProbability.get(note) || 0;
        totalChange += Math.abs(newProb - oldProb);
      }

      converged = totalChange < this.convergenceThreshold;
      probability = newProbability;
      iteration++;
    }

    if (converged) {
      console.error(
        `[GraphProximityManager] Converged after ${iteration} iterations`
      );
    } else {
      console.error(
        `[GraphProximityManager] Reached max iterations (${this.maxIterations})`
      );
    }

    // Convert probabilities to proximity scores (normalize by max)
    const scores: ProximityScores = new Map();
    const maxProb = Math.max(...Array.from(probability.values()));

    if (maxProb > 0) {
      for (const [note, prob] of probability.entries()) {
        if (note !== seedNote && prob > 0) {
          // Exclude seed note itself from results
          scores.set(note, prob / maxProb);
        }
      }
    }

    // Cache the results
    this.cache.set(seedNote, forwardLinks, backlinks, scores);

    console.error(
      `[GraphProximityManager] Computed proximity to ${scores.size} notes`
    );

    return scores;
  }

  /**
   * Compute multi-seed proximity using intersection (multiply scores)
   *
   * For a note to have high proximity, it must be close to ALL seeds.
   * This is implemented by multiplying the proximity scores from each seed.
   *
   * @param seedNotes - Array of seed notes
   * @returns Combined proximity scores
   */
  async computeMultiSeedProximity(
    seedNotes: string[]
  ): Promise<ProximityScores> {
    if (seedNotes.length === 0) {
      return new Map();
    }

    if (seedNotes.length === 1) {
      return this.computeProximity(seedNotes[0]);
    }

    console.error(
      `[GraphProximityManager] Computing multi-seed proximity from ${seedNotes.length} seeds`
    );

    // Compute proximity from each seed
    const proximities = await Promise.all(
      seedNotes.map((seed) => this.computeProximity(seed))
    );

    // Multiply scores (intersection - must be close to ALL seeds)
    const combined = new Map<string, number>();
    const allNotes = this.graphIndex.getAllNotes();

    for (const note of allNotes) {
      // Skip seed notes themselves
      if (seedNotes.includes(note)) {
        continue;
      }

      // Multiply proximity scores from all seeds
      let score = 1.0;
      for (const proximity of proximities) {
        score *= proximity.get(note) || 0;
      }

      if (score > 0) {
        combined.set(note, score);
      }
    }

    console.error(
      `[GraphProximityManager] Combined proximity to ${combined.size} notes`
    );

    return combined;
  }

  /**
   * Invalidate cache for a specific note
   */
  invalidate(note: string): void {
    this.cache.invalidate(note);
  }

  /**
   * Save cache to disk
   */
  async saveCache(): Promise<void> {
    await this.cache.save();
  }

  /**
   * Get cache statistics
   */
  getCacheStats() {
    return this.cache.getStats();
  }
}
