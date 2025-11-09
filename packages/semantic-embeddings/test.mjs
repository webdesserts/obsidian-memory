// Quick test of the semantic embeddings package

import { SemanticEmbeddings } from './pkg/semantic_embeddings.js';
import { readFileSync } from 'fs';
import { fileURLToPath } from 'url';
import { dirname, join } from 'path';

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);

async function test() {
  console.log('Loading model files...');

  // Load model files from disk
  const modelDir = join(__dirname, 'models', 'all-MiniLM-L6-v2');
  const configJson = readFileSync(join(modelDir, 'config.json'), 'utf-8');
  const tokenizerJson = readFileSync(join(modelDir, 'tokenizer.json'), 'utf-8');
  const modelWeights = readFileSync(join(modelDir, 'model.safetensors'));

  console.log(`Loaded config (${configJson.length} bytes)`);
  console.log(`Loaded tokenizer (${tokenizerJson.length} bytes)`);
  console.log(`Loaded weights (${(modelWeights.length / 1024 / 1024).toFixed(2)} MB)`);

  console.log('\nCreating SemanticEmbeddings instance...');
  const embeddings = new SemanticEmbeddings();

  console.log('Initializing model...');
  await embeddings.loadModel(configJson, tokenizerJson, modelWeights);

  console.log('\nEncoding test text...');
  const text = "Machine learning is a subset of artificial intelligence.";
  const vector = await embeddings.encode(text);

  console.log(`✓ Generated embedding with ${vector.length} dimensions`);
  console.log(`  First 5 values: [${Array.from(vector.slice(0, 5)).map(v => v.toFixed(4)).join(', ')}]`);

  console.log('\nTesting batch encoding...');
  const texts = [
    "Machine learning is fascinating",
    "I love pizza and pasta",
    "AI and deep learning are related"
  ];
  const vectors = await embeddings.encodeBatch(texts);
  console.log(`✓ Batch encoded ${vectors.length} texts`);

  console.log('\nTesting similarity computation...');
  const sim1 = embeddings.cosineSimilarity(vectors[0], vectors[2]);  // ML vs AI
  const sim2 = embeddings.cosineSimilarity(vectors[0], vectors[1]);  // ML vs pizza

  console.log(`  ML vs AI similarity: ${(sim1 * 100).toFixed(1)}%`);
  console.log(`  ML vs Pizza similarity: ${(sim2 * 100).toFixed(1)}%`);

  if (sim1 > sim2) {
    console.log('  ✓ ML is more similar to AI than pizza (as expected)');
  } else {
    console.log('  ✗ Unexpected: pizza seems more similar to ML than AI!');
  }

  console.log('\nTesting findMostSimilar...');
  const queryVector = vectors[0];
  const topIndices = embeddings.findMostSimilar(queryVector, vectors, 2);
  console.log(`  Top 2 most similar indices: [${topIndices}]`);
  console.log(`  Expected: [0, 2] (ML itself, then AI)`);

  console.log('\n✅ All tests completed!');
}

test().catch(err => {
  console.error('❌ Test failed:', err);
  process.exit(1);
});
