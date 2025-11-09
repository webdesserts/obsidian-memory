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
