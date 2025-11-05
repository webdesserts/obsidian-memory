#!/usr/bin/env node

/**
 * Download DeepSeek-R1-Distill-Qwen-1.5B model if not already present
 *
 * This runs automatically during npm install via the prepare script.
 * Skip by setting SKIP_MODEL_DOWNLOAD=1 environment variable.
 */

import { existsSync, mkdirSync, createWriteStream } from 'fs';
import { join, dirname } from 'path';
import { fileURLToPath } from 'url';
import { pipeline } from 'stream/promises';

const __dirname = dirname(fileURLToPath(import.meta.url));

// Check if we should skip download
if (process.env.SKIP_MODEL_DOWNLOAD === '1') {
  console.log('[model] Skipping model download (SKIP_MODEL_DOWNLOAD=1)');
  process.exit(0);
}

const modelsDir = join(__dirname, '../models');
const modelPath = join(modelsDir, 'DeepSeek-R1-Distill-Qwen-14B-Q4_K_M.gguf');

// Check if model already exists
if (existsSync(modelPath)) {
  console.log('[model] Model already exists, skipping download');
  process.exit(0);
}

// Ensure models directory exists
if (!existsSync(modelsDir)) {
  mkdirSync(modelsDir, { recursive: true });
}

console.log('[model] Downloading DeepSeek-R1-Distill-Qwen-14B-Q4_K_M (~8.99 GB)...');
console.log('[model] This may take several minutes on first install');

const modelUrl = 'https://huggingface.co/bartowski/DeepSeek-R1-Distill-Qwen-14B-GGUF/resolve/main/DeepSeek-R1-Distill-Qwen-14B-Q4_K_M.gguf';

try {
  const response = await fetch(modelUrl);

  if (!response.ok) {
    throw new Error(`HTTP error! status: ${response.status}`);
  }

  const totalSize = parseInt(response.headers.get('content-length') || '0', 10);
  let downloadedSize = 0;
  let lastPercent = 0;

  // Create write stream
  const fileStream = createWriteStream(modelPath);

  // Track progress
  const reader = response.body.getReader();
  const chunks = [];

  while (true) {
    const { done, value } = await reader.read();

    if (done) break;

    downloadedSize += value.length;
    chunks.push(value);

    // Log progress every 10%
    const percent = Math.round((downloadedSize / totalSize) * 100);
    if (percent >= lastPercent + 10) {
      console.log(`[model] Downloaded ${percent}%...`);
      lastPercent = percent;
    }
  }

  // Write all chunks to file
  for (const chunk of chunks) {
    fileStream.write(chunk);
  }

  fileStream.end();

  await new Promise((resolve, reject) => {
    fileStream.on('finish', resolve);
    fileStream.on('error', reject);
  });

  console.log('[model] Download complete!');
} catch (error) {
  console.error('[model] Failed to download model:', error.message);
  console.error('[model] You can download manually from:');
  console.error(`[model]   ${modelUrl}`);
  console.error(`[model] Save it to: ${modelPath}`);
  // Don't fail the install if model download fails
  process.exit(0);
}
