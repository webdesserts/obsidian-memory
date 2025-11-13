/**
 * Embedding vector (384 dimensions for all-MiniLM-L6-v2)
 */
export type EmbeddingVector = Float32Array;

/**
 * Cached embedding entry with metadata
 */
export interface CachedEmbedding {
  /** SHA-256 hash of file content */
  contentHash: string;
  /** Embedding vector */
  embedding: number[];
  /** When the embedding was computed */
  timestamp: string;
  /** File path relative to vault */
  path: string;
}

/**
 * Cache metadata for embeddings
 */
export interface EmbeddingCacheMetadata {
  /** Model name/version used for embeddings */
  model: string;
  /** When cache was created */
  created: string;
  /** When cache was last updated */
  lastUpdated: string;
  /** Number of cached embeddings */
  count: number;
}

/**
 * Persisted cache structure on disk
 */
export interface EmbeddingCacheData {
  metadata: EmbeddingCacheMetadata;
  /** Map of file path to cached embedding */
  embeddings: Record<string, CachedEmbedding>;
}

/**
 * Graph proximity scores for a single seed note
 * Maps target note names to proximity scores (0.0-1.0)
 */
export type ProximityScores = Map<string, number>;

/**
 * Cached graph proximity entry
 */
export interface CachedProximity {
  /** SHA-256 hash of link signature (sorted links) */
  linkHash: string;
  /** Proximity scores to other notes */
  scores: Record<string, number>;
  /** When proximity was computed */
  timestamp: string;
  /** Seed note this proximity was computed from */
  seedNote: string;
}

/**
 * Cache metadata for graph proximity
 */
export interface GraphProximityCacheMetadata {
  /** Algorithm parameters */
  algorithm: {
    name: string;
    restart: number;
    convergence: number;
    maxIterations: number;
  };
  /** When cache was created */
  created: string;
  /** When cache was last updated */
  lastUpdated: string;
  /** Number of cached proximity computations */
  count: number;
}

/**
 * Persisted graph proximity cache structure
 */
export interface GraphProximityCacheData {
  metadata: GraphProximityCacheMetadata;
  /** Map of seed note to cached proximity scores */
  proximities: Record<string, CachedProximity>;
}
