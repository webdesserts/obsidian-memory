import fs from "fs/promises";
import path from "path";
import { fileURLToPath } from "url";
import { EmbeddingCache } from "./cache.js";
import type { EmbeddingVector } from "./types.js";
import type { SemanticEmbeddings } from "../../../semantic-embeddings/pkg/semantic_embeddings.js";

/**
 * Search result with similarity score
 */
export interface SearchResult {
  filePath: string;
  similarity: number;
}

/**
 * Prepare content for embedding by prepending the note title and frontmatter
 *
 * This ensures notes can be found by:
 * - Title (even if they have little content)
 * - Aliases (alternative names for the note)
 * - Other frontmatter fields that provide searchable context
 *
 * Must be used consistently by both warmup and search to ensure cache hits.
 *
 * @param noteName - The name of the note (used as title)
 * @param content - The note's content (without frontmatter)
 * @param frontmatter - Optional parsed frontmatter object
 * @returns Content with title and frontmatter prepended
 */
export function prepareContentForEmbedding(
  noteName: string,
  content: string,
  frontmatter?: Record<string, any>
): string {
  let prepared = noteName;

  // Add aliases if present (common pattern: aliases: [name1, name2])
  if (frontmatter?.aliases) {
    const aliases = Array.isArray(frontmatter.aliases)
      ? frontmatter.aliases
      : [frontmatter.aliases];
    if (aliases.length > 0) {
      prepared += `\nAliases: ${aliases.join(", ")}`;
    }
  }

  // Add tags if present (helps with topic categorization)
  if (frontmatter?.tags) {
    const tags = Array.isArray(frontmatter.tags)
      ? frontmatter.tags
      : [frontmatter.tags];
    if (tags.length > 0) {
      prepared += `\nTags: ${tags.join(", ")}`;
    }
  }

  return `${prepared}\n\n${content}`;
}

/**
 * Manager for semantic embeddings - coordinates WASM module and cache
 *
 * Handles model loading, embedding computation with caching, and similarity search.
 * Uses singleton pattern for resource efficiency.
 */
export class EmbeddingManager {
  private static instance: EmbeddingManager | null = null;
  private static instancePromise: Promise<EmbeddingManager> | null = null;
  private embeddings: SemanticEmbeddings | null = null;
  private cache: EmbeddingCache;
  private modelLoaded = false;
  private modelName = "all-MiniLM-L6-v2";

  private constructor(vaultPath: string) {
    this.cache = new EmbeddingCache(vaultPath, this.modelName);
  }

  /**
   * Get or create singleton instance
   *
   * Thread-safe: If multiple calls occur simultaneously, they all receive
   * the same instance after a single initialization.
   */
  static async getInstance(vaultPath: string): Promise<EmbeddingManager> {
    // Return existing instance if already initialized
    if (EmbeddingManager.instance) {
      return EmbeddingManager.instance;
    }

    // If initialization is in progress, wait for it
    if (EmbeddingManager.instancePromise) {
      return EmbeddingManager.instancePromise;
    }

    // Start new initialization
    // Use instancePromise pattern: concurrent calls to getInstance() wait
    // for the same initialization instead of creating multiple instances.
    // The promise is stored before starting async work to handle race conditions.
    EmbeddingManager.instancePromise = (async () => {
      const manager = new EmbeddingManager(vaultPath);
      await manager.initialize();
      EmbeddingManager.instance = manager;
      EmbeddingManager.instancePromise = null; // Clear promise after completion
      return manager;
    })();

    return EmbeddingManager.instancePromise;
  }

  /**
   * Reset singleton instance (for testing)
   */
  static resetInstance(): void {
    EmbeddingManager.instance = null;
    EmbeddingManager.instancePromise = null;
  }

  /**
   * Initialize the embedding system - load cache and model
   */
  private async initialize(): Promise<void> {
    try {
      console.error("[EmbeddingManager] Initializing...");

      // Load cache from disk
      await this.cache.load();

      // Load WASM module
      await this.loadWasmModule();

      console.error("[EmbeddingManager] Initialization complete");
    } catch (error) {
      // Clean up singleton state on failure to allow retry
      EmbeddingManager.instancePromise = null;
      throw error;
    }
  }

  /**
   * Pre-encode all notes at startup to warm up cache
   *
   * This encodes all notes that don't have cached embeddings yet,
   * making first search instant. Run this after initialization.
   */
  async warmupCache(vaultPath: string, graphIndex: any, fileOps: any): Promise<void> {
    console.error("[EmbeddingManager] Warming up cache...");

    const allNotes = graphIndex.getAllNotes();
    const notesToEncode: Array<{ filePath: string; content: string }> = [];

    // Load all notes that aren't already cached
    for (const noteName of allNotes) {
      const notePath = graphIndex.getNotePath(noteName);
      if (!notePath) continue;

      try {
        const { content, frontmatter } = await fileOps.readNote(notePath);

        // Prepare content with title and frontmatter for embedding
        const preparedContent = prepareContentForEmbedding(noteName, content, frontmatter);

        // Check if this content is already cached
        const cached = await this.cache.get(notePath, preparedContent);
        if (cached) continue; // Already cached with correct hash

        notesToEncode.push({ filePath: notePath, content: preparedContent });
      } catch (error) {
        console.error(`[EmbeddingManager] Error reading ${notePath}: ${error}`);
      }
    }

    if (notesToEncode.length === 0) {
      console.error("[EmbeddingManager] Cache already warm (no new notes to encode)");
      return;
    }

    console.error(`[EmbeddingManager] Encoding ${notesToEncode.length} uncached notes...`);

    // Batch encode all uncached notes
    await this.encodeNotes(notesToEncode);

    // Save cache to disk
    await this.cache.save();

    console.error(`[EmbeddingManager] Cache warmup complete`);
  }

  /**
   * Load WASM module and model files
   */
  private async loadWasmModule(): Promise<void> {
    try {
      // Import WASM module
      // Get path to semantic-embeddings package
      const __filename = fileURLToPath(import.meta.url);
      const __dirname = path.dirname(__filename);
      const wasmPath = path.resolve(
        __dirname,
        "../../../semantic-embeddings/pkg/semantic_embeddings.js"
      );

      const wasmModule = await import(wasmPath);

      this.embeddings = new wasmModule.SemanticEmbeddings();

      // Load model files
      const modelDir = path.resolve(
        __dirname,
        "../../../semantic-embeddings/models",
        this.modelName
      );

      console.error(`[EmbeddingManager] Loading model from ${modelDir}...`);

      // Validate model files exist before attempting to load
      const requiredFiles = ["config.json", "tokenizer.json", "model.safetensors"];
      const missingFiles: string[] = [];

      for (const file of requiredFiles) {
        try {
          await fs.access(path.join(modelDir, file));
        } catch {
          missingFiles.push(file);
        }
      }

      if (missingFiles.length > 0) {
        throw new Error(
          `Missing model files: ${missingFiles.join(", ")}\n` +
          `Run: cd packages/semantic-embeddings && npm run download-model`
        );
      }

      const [configJson, tokenizerJson, modelWeights] = await Promise.all([
        fs.readFile(path.join(modelDir, "config.json"), "utf-8"),
        fs.readFile(path.join(modelDir, "tokenizer.json"), "utf-8"),
        fs.readFile(path.join(modelDir, "model.safetensors")),
      ]);

      // loadModel is synchronous (WASM binding)
      this.embeddings!.loadModel(configJson, tokenizerJson, modelWeights);

      this.modelLoaded = true;

      console.error(`[EmbeddingManager] Model loaded successfully`);
    } catch (error) {
      console.error(`[EmbeddingManager] Failed to load WASM module: ${error}`);
      throw error;
    }
  }

  /**
   * Encode text into an embedding vector (without caching)
   *
   * For single text: encodes directly
   * For multiple texts: encodes each and averages the vectors
   *
   * Use this for query text or other non-file content
   *
   * @param text - Single text string or array of text strings to encode
   * @returns Embedding vector (averaged if multiple texts provided)
   */
  async encode(text: string | string[]): Promise<EmbeddingVector> {
    if (!this.modelLoaded || !this.embeddings) {
      throw new Error("Model not loaded. Call initialize() first.");
    }

    // Single text - encode directly
    if (typeof text === 'string') {
      return await this.embeddings.encode(text);
    }

    // Multiple texts - encode and average
    if (text.length === 0) {
      throw new Error("Cannot encode empty array of texts");
    }

    if (text.length === 1) {
      return await this.embeddings.encode(text[0]);
    }

    // Encode all texts
    const vectors = await Promise.all(
      text.map(t => this.embeddings!.encode(t))
    );

    // Average vectors
    return this.averageVectors(vectors);
  }

  /**
   * Encode a single note's content into an embedding vector
   *
   * Uses cache if content hasn't changed, otherwise computes and caches
   */
  async encodeNote(
    filePath: string,
    content: string
  ): Promise<EmbeddingVector> {
    if (!this.modelLoaded || !this.embeddings) {
      throw new Error("Model not loaded. Call initialize() first.");
    }

    // Check cache first
    const cached = await this.cache.get(filePath, content);
    if (cached) {
      return cached;
    }

    // Cache miss - compute embedding
    const embedding = await this.embeddings.encode(content);

    // Store in cache
    this.cache.set(filePath, content, embedding);

    return embedding;
  }

  /**
   * Encode multiple notes in batch (more efficient than individual encodes)
   *
   * Automatically uses cache for unchanged files
   */
  async encodeNotes(
    notes: Array<{ filePath: string; content: string }>
  ): Promise<Map<string, EmbeddingVector>> {
    if (!this.modelLoaded || !this.embeddings) {
      throw new Error("Model not loaded. Call initialize() first.");
    }

    const results = new Map<string, EmbeddingVector>();
    const toCompute: Array<{ filePath: string; content: string; index: number }> = [];

    // Check cache for each note
    for (let i = 0; i < notes.length; i++) {
      const { filePath, content } = notes[i];
      const cached = await this.cache.get(filePath, content);

      if (cached) {
        results.set(filePath, cached);
      } else {
        toCompute.push({ filePath, content, index: i });
      }
    }

    // Batch compute uncached embeddings
    if (toCompute.length > 0) {
      console.error(
        `[EmbeddingManager] Computing ${toCompute.length} embeddings (${results.size} from cache)`
      );

      const texts = toCompute.map((item) => item.content);
      const embeddings = await this.embeddings.encodeBatch(texts);

      // Store results and update cache
      for (let i = 0; i < toCompute.length; i++) {
        const { filePath, content } = toCompute[i];
        const embedding = embeddings[i];

        this.cache.set(filePath, content, embedding);
        results.set(filePath, embedding);
      }
    }

    return results;
  }

  /**
   * Find most similar notes to a query
   *
   * @param query - Query text to search for
   * @param candidateEmbeddings - Map of file paths to their embeddings
   * @param topK - Number of results to return
   */
  async search(
    query: string,
    candidateEmbeddings: Map<string, EmbeddingVector>,
    topK = 10
  ): Promise<SearchResult[]> {
    if (!this.modelLoaded || !this.embeddings) {
      throw new Error("Model not loaded. Call initialize() first.");
    }

    // Encode query
    const queryEmbedding = await this.embeddings.encode(query);

    return this.searchWithVector(queryEmbedding, candidateEmbeddings, topK);
  }

  /**
   * Find most similar notes using a pre-computed embedding vector
   *
   * @param queryEmbedding - Pre-computed query embedding vector
   * @param candidateEmbeddings - Map of file paths to their embeddings
   * @param topK - Number of results to return
   */
  searchWithVector(
    queryEmbedding: EmbeddingVector,
    candidateEmbeddings: Map<string, EmbeddingVector>,
    topK = 10
  ): SearchResult[] {
    if (!this.modelLoaded || !this.embeddings) {
      throw new Error("Model not loaded. Call initialize() first.");
    }

    // Convert Map to arrays for WASM
    const filePaths = Array.from(candidateEmbeddings.keys());
    const embeddings = Array.from(candidateEmbeddings.values());

    // Find top K most similar
    const indices = this.embeddings.findMostSimilar(
      queryEmbedding,
      embeddings,
      topK
    );

    // Convert indices to results with similarity scores
    const results: SearchResult[] = [];
    for (const index of indices) {
      const filePath = filePaths[index];
      const embedding = embeddings[index];
      const similarity = this.embeddings.cosineSimilarity(
        queryEmbedding,
        embedding
      );

      results.push({ filePath, similarity });
    }

    return results;
  }

  /**
   * Average multiple embedding vectors
   *
   * Used for multi-note queries where we want to find notes similar to ALL referenced notes.
   * Averaging vectors produces a vector equidistant from all inputs.
   *
   * @param vectors - Array of embedding vectors to average
   * @returns Averaged embedding vector
   */
  averageVectors(vectors: EmbeddingVector[]): EmbeddingVector {
    if (vectors.length === 0) {
      throw new Error("Cannot average empty array of vectors");
    }

    if (vectors.length === 1) {
      return vectors[0];
    }

    // All vectors must have same dimension
    const dimension = vectors[0].length;
    const averaged = new Float32Array(dimension);

    // Sum all vectors
    for (const vector of vectors) {
      if (vector.length !== dimension) {
        throw new Error(`Vector dimension mismatch: expected ${dimension}, got ${vector.length}`);
      }
      for (let i = 0; i < dimension; i++) {
        averaged[i] += vector[i];
      }
    }

    // Divide by count to get average
    for (let i = 0; i < dimension; i++) {
      averaged[i] /= vectors.length;
    }

    return averaged;
  }

  /**
   * Invalidate cache for a specific file
   */
  invalidate(filePath: string): void {
    this.cache.invalidate(filePath);
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
