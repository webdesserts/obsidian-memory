# @webdesserts/obsidian-memory-semantic-embeddings

Rust-based semantic embeddings for Obsidian notes using Candle and sentence transformers.

## Features

- **Fast local inference** - No external API calls, runs entirely on your machine
- **Efficient batching** - Process multiple notes at once for better performance
- **Small model size** - all-MiniLM-L6-v2 model (~80MB download)
- **Native performance** - Rust + napi-rs for maximum speed

## Installation

```bash
npm install @webdesserts/obsidian-memory-semantic-embeddings
```

## Usage

```typescript
import { SemanticEmbeddings } from '@webdesserts/obsidian-memory-semantic-embeddings';

const embeddings = new SemanticEmbeddings();

// Encode single text
const vector = await embeddings.encode("This is a note about machine learning");

// Encode multiple texts (more efficient)
const vectors = await embeddings.encodeBatch([
  "First note content",
  "Second note content",
  "Third note content"
]);

// Compute similarity
const similarity = embeddings.cosineSimilarity(vector1, vector2);
console.log(`Similarity: ${(similarity * 100).toFixed(1)}%`);

// Find most similar
const query = await embeddings.encode("machine learning");
const topIndices = embeddings.findMostSimilar(query, allVectors, 10);
```

## API

### `SemanticEmbeddings`

#### `encode(text: string): Promise<Float32Array>`

Encode a single text into an embedding vector (384 dimensions).

#### `encodeBatch(texts: string[]): Promise<Float32Array[]>`

Encode multiple texts in batch. More efficient than calling `encode()` multiple times.

#### `cosineSimilarity(a: Float32Array, b: Float32Array): number`

Compute cosine similarity between two embedding vectors. Returns a value between -1 and 1 (typically 0 to 1 for similar texts).

#### `findMostSimilar(query: Float32Array, candidates: Float32Array[], topK: number): number[]`

Find indices of top K most similar embeddings to the query. Returns indices sorted by similarity (descending).

## Model

Uses **all-MiniLM-L6-v2** sentence transformer model:
- Embedding dimension: 384
- Model size: ~80MB
- License: Apache 2.0

The model is downloaded automatically on first use and cached in `models/all-MiniLM-L6-v2/`.

## Development

```bash
# Build Rust code and generate TypeScript bindings
npm run build

# Build in debug mode (faster compilation)
npm run build:debug

# Watch mode for development
npm run dev

# Run tests
npm test
```

## License

MIT
