#!/usr/bin/env node

/**
 * Download all-MiniLM-L6-v2 sentence transformer model from Hugging Face
 *
 * Downloads to: models/all-MiniLM-L6-v2/
 */

import fs from 'fs';
import path from 'path';
import { fileURLToPath } from 'url';
import https from 'https';
import http from 'http';

const __dirname = path.dirname(fileURLToPath(import.meta.url));
const MODEL_DIR = path.join(__dirname, '..', 'models', 'all-MiniLM-L6-v2');

// Hugging Face model repository
const REPO = 'sentence-transformers/all-MiniLM-L6-v2';
const BASE_URL = `https://huggingface.co/${REPO}/resolve/main`;

// Files to download
const FILES = [
  'config.json',
  'tokenizer.json',
  'tokenizer_config.json',
  'vocab.txt',
  'model.safetensors'
];

// File sizes (approximate, for progress display)
const FILE_SIZES = {
  'config.json': '1 KB',
  'tokenizer.json': '466 KB',
  'tokenizer_config.json': '1 KB',
  'vocab.txt': '232 KB',
  'model.safetensors': '90 MB'
};

function downloadFile(url, dest, maxRedirects = 5) {
  return new Promise((resolve, reject) => {
    if (maxRedirects <= 0) {
      return reject(new Error('Too many redirects'));
    }

    const file = fs.createWriteStream(dest);
    const protocol = url.startsWith('https') ? https : http;

    const request = protocol.get(url, (response) => {
      // Handle redirects
      if (response.statusCode === 301 || response.statusCode === 302 || response.statusCode === 307) {
        let redirectUrl = response.headers.location;

        // Handle relative redirects
        if (!redirectUrl.startsWith('http')) {
          const urlObj = new URL(url);
          redirectUrl = `${urlObj.protocol}//${urlObj.host}${redirectUrl}`;
        }

        file.close();
        fs.unlinkSync(dest); // Remove empty file

        console.log(`  Following redirect...`);
        return downloadFile(redirectUrl, dest, maxRedirects - 1)
          .then(resolve)
          .catch(reject);
      }

      if (response.statusCode !== 200) {
        file.close();
        fs.unlinkSync(dest);
        return reject(new Error(`HTTP ${response.statusCode}: ${response.statusMessage}`));
      }

      const totalBytes = parseInt(response.headers['content-length'], 10);
      let downloadedBytes = 0;

      response.pipe(file);

      response.on('data', (chunk) => {
        downloadedBytes += chunk.length;
        if (totalBytes) {
          const percent = ((downloadedBytes / totalBytes) * 100).toFixed(1);
          process.stdout.write(`\r  Progress: ${percent}%`);
        }
      });

      file.on('finish', () => {
        file.close();
        process.stdout.write('\n');
        resolve();
      });

      response.on('error', (err) => {
        file.close();
        fs.unlink(dest, () => {});
        reject(err);
      });
    });

    request.on('error', (err) => {
      file.close();
      fs.unlink(dest, () => {});
      reject(err);
    });

    file.on('error', (err) => {
      fs.unlink(dest, () => {});
      reject(err);
    });
  });
}

async function main() {
  console.log('Downloading all-MiniLM-L6-v2 model from Hugging Face...\n');

  // Create model directory if it doesn't exist
  if (!fs.existsSync(MODEL_DIR)) {
    fs.mkdirSync(MODEL_DIR, { recursive: true });
    console.log(`Created directory: ${MODEL_DIR}\n`);
  }

  // Check if model already downloaded
  const allFilesExist = FILES.every(file => {
    const filePath = path.join(MODEL_DIR, file);
    if (!fs.existsSync(filePath)) return false;

    // Check file size is > 100 bytes (avoids redirect HTML)
    const stats = fs.statSync(filePath);
    return stats.size > 100;
  });

  if (allFilesExist) {
    console.log('✓ Model already downloaded\n');
    return;
  }

  // Download each file
  for (const file of FILES) {
    const destPath = path.join(MODEL_DIR, file);

    // Skip if already exists and is valid
    if (fs.existsSync(destPath)) {
      const stats = fs.statSync(destPath);
      if (stats.size > 100) {
        console.log(`✓ ${file} (already exists)`);
        continue;
      } else {
        // Remove invalid file
        fs.unlinkSync(destPath);
      }
    }

    const url = `${BASE_URL}/${file}`;
    const size = FILE_SIZES[file] || 'unknown size';

    console.log(`Downloading ${file} (${size})...`);

    try {
      await downloadFile(url, destPath);
      console.log(`✓ ${file}\n`);
    } catch (err) {
      console.error(`✗ Failed to download ${file}:`, err.message);
      process.exit(1);
    }
  }

  console.log('\n✓ Model download complete!');
  console.log(`\nModel saved to: ${MODEL_DIR}`);
}

main().catch(err => {
  console.error('Error:', err);
  process.exit(1);
});
