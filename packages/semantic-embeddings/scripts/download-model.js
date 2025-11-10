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
    // Still run optimization in case tokenizer.json needs updating
  } else {

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
  }

  // Remove fixed padding configuration from tokenizer.json
  //
  // Hugging Face's tokenizer.json contains `padding: { strategy: { Fixed: 128 } }`, which pads
  // all sequences to exactly 128 tokens. However, Python's sentence-transformers library ignores
  // this config and uses `padding=False` by default, processing only the actual token count (3-10).
  //
  // Candle's BERT implementation is sensitive to sequence length even with attention masking -
  // processing 128 positions produces different embeddings than processing 3 positions, despite
  // padding tokens being masked. The sequence length affects positional embeddings, layer norms,
  // and attention patterns throughout BERT's 6 transformer layers.
  //
  // Removing the padding config aligns our tokenizer behavior with Python's actual usage,
  // significantly improving embedding accuracy and reducing memory usage.
  const tokenizerPath = path.join(MODEL_DIR, 'tokenizer.json');
  const versionPath = path.join(MODEL_DIR, '.tokenizer-version');

  console.log('\nOptimizing tokenizer.json...');

  try {
    const tokenizerData = JSON.parse(fs.readFileSync(tokenizerPath, 'utf8'));

    // Track tokenizer version to detect upstream changes
    const tokenizerVersion = tokenizerData.version || 'unknown';
    let lastKnownVersion = 'unknown';

    if (fs.existsSync(versionPath)) {
      lastKnownVersion = fs.readFileSync(versionPath, 'utf8').trim();
    }

    if (lastKnownVersion !== 'unknown' && tokenizerVersion !== lastKnownVersion) {
      console.log('');
      console.log('⚠️  ALERT: Tokenizer version changed!');
      console.log(`   Previous version: ${lastKnownVersion}`);
      console.log(`   New version:      ${tokenizerVersion}`);
      console.log('   Please verify that our padding optimization is still appropriate.');
      console.log('   Compare the new padding config against our optimization.');
      console.log('');
    }

    // Save current version
    fs.writeFileSync(versionPath, tokenizerVersion);

    if (tokenizerData.padding) {
      console.log('  Removing fixed padding configuration:');
      console.log(`    Was: Fixed padding to 128 tokens`);
      console.log(`    Now: Variable length (actual token count)`);
      console.log(`  Tokenizer version: ${tokenizerVersion}`);
      delete tokenizerData.padding;
      fs.writeFileSync(tokenizerPath, JSON.stringify(tokenizerData, null, 2));
      console.log('✓ Tokenizer optimized - Rust/Candle now matches Python/PyTorch behavior');
    } else {
      console.log('✓ Tokenizer already optimized');
      console.log(`  Tokenizer version: ${tokenizerVersion}`);
    }
  } catch (err) {
    console.warn('⚠ Warning: Could not optimize tokenizer.json:', err.message);
  }

  console.log(`\nModel saved to: ${MODEL_DIR}`);
}

main().catch(err => {
  console.error('Error:', err);
  process.exit(1);
});
