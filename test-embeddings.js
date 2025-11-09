#!/usr/bin/env node

import { fileURLToPath } from "url";
import { dirname, join } from "path";
import { readFile } from "fs/promises";

const __filename = fileURLToPath(import.meta.url);
const __dirname = dirname(__filename);

async function test() {
  console.log("Loading WASM module...");

  const wasmPath = join(__dirname, "packages/semantic-embeddings/pkg/semantic_embeddings.js");
  const wasmModule = await import(wasmPath);

  console.log("Creating SemanticEmbeddings instance...");
  const embeddings = new wasmModule.SemanticEmbeddings();

  // Load model files
  const modelDir = join(__dirname, "packages/semantic-embeddings/models/all-MiniLM-L6-v2");

  console.log("Loading model files...");
  const [configJson, tokenizerJson, modelWeights] = await Promise.all([
    readFile(join(modelDir, "config.json"), "utf-8"),
    readFile(join(modelDir, "tokenizer.json"), "utf-8"),
    readFile(join(modelDir, "model.safetensors")),
  ]);

  console.log("Loading model into WASM...");
  embeddings.loadModel(configJson, tokenizerJson, modelWeights);

  // Test embeddings
  console.log("\nTesting embeddings...");

  const text1 = "MCP Servers";
  const text2 = "Model Context Protocol server implementation";
  const text3 = "Musical intervals like Major 2nd and Minor 7th";

  console.log(`\nEncoding: "${text1}"`);
  const emb1 = embeddings.encode(text1);
  console.log(`Length: ${emb1.length}, First few values:`, emb1.slice(0, 5));

  console.log(`\nEncoding: "${text2}"`);
  const emb2 = embeddings.encode(text2);
  console.log(`Length: ${emb2.length}, First few values:`, emb2.slice(0, 5));

  console.log(`\nEncoding: "${text3}"`);
  const emb3 = embeddings.encode(text3);
  console.log(`Length: ${emb3.length}, First few values:`, emb3.slice(0, 5));

  // Calculate similarities
  const sim12 = embeddings.cosineSimilarity(emb1, emb2);
  const sim13 = embeddings.cosineSimilarity(emb1, emb3);
  const sim23 = embeddings.cosineSimilarity(emb2, emb3);

  console.log("\nSimilarity scores:");
  console.log(`"${text1}" vs "${text2}": ${(sim12 * 100).toFixed(1)}%`);
  console.log(`"${text1}" vs "${text3}": ${(sim13 * 100).toFixed(1)}%`);
  console.log(`"${text2}" vs "${text3}": ${(sim23 * 100).toFixed(1)}%`);

  console.log("\nExpected: MCP-related texts should be more similar to each other than to music theory");
}

test().catch(console.error);
