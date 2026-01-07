#!/usr/bin/env bash
set -euo pipefail

# Download all-MiniLM-L6-v2 model from HuggingFace
#
# This script downloads the model files needed for the embedded-model feature.
# It also pre-optimizes tokenizer.json by removing the padding config.
#
# Usage:
#   ./scripts/download-model.sh
#
# The model files are saved to:
#   crates/semantic-embeddings/models/all-MiniLM-L6-v2/

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
PROJECT_ROOT="$(cd "$SCRIPT_DIR/.." && pwd)"
MODEL_DIR="$PROJECT_ROOT/crates/semantic-embeddings/models/all-MiniLM-L6-v2"

REPO="sentence-transformers/all-MiniLM-L6-v2"
BASE_URL="https://huggingface.co/${REPO}/resolve/main"

# Files to download
FILES=(
    "config.json"
    "tokenizer.json"
    "tokenizer_config.json"
    "vocab.txt"
    "model.safetensors"
)

# Expected SHA256 checksums (for verification)
# These are for the main branch as of Jan 2026
declare -A CHECKSUMS=(
    ["config.json"]="953f9c0d463486b10a6871cc2fd59f223b2c70184f49815e7efbcab5d8908b41"
    ["tokenizer.json"]="be50c3628f2bf5bb5e3a7f17b1f74611b2561a3a27eeab05e5aa30f411572037"
    ["tokenizer_config.json"]="acb92769e8195aabd29b7b2137a9e6d6e25c476a4f15aa4355c233426c61576b"
    ["vocab.txt"]="07eced375cec144d27c900241f3e339478dec958f92fddbc551f295c992038a3"
    ["model.safetensors"]="53aa51172d142c89d9012cce15ae4d6cc0ca6895895114379cacb4fab128d9db"
)

echo "Downloading all-MiniLM-L6-v2 model from HuggingFace..."
echo "Target directory: $MODEL_DIR"
echo

mkdir -p "$MODEL_DIR"

# Download each file
for file in "${FILES[@]}"; do
    dest="$MODEL_DIR/$file"
    
    # Check if file already exists and is valid
    if [[ -f "$dest" ]]; then
        size=$(stat -f%z "$dest" 2>/dev/null || stat -c%s "$dest" 2>/dev/null || echo "0")
        if [[ "$size" -gt 100 ]]; then
            echo "✓ $file (already exists)"
            continue
        fi
        # Remove invalid/empty file
        rm -f "$dest"
    fi
    
    echo "Downloading $file..."
    curl -fSL --progress-bar "${BASE_URL}/${file}" -o "$dest"
    
    # Verify checksum if we have one
    if [[ -n "${CHECKSUMS[$file]:-}" ]]; then
        echo -n "  Verifying checksum... "
        if command -v sha256sum &> /dev/null; then
            actual=$(sha256sum "$dest" | cut -d' ' -f1)
        elif command -v shasum &> /dev/null; then
            actual=$(shasum -a 256 "$dest" | cut -d' ' -f1)
        else
            echo "skipped (no sha256sum/shasum available)"
            continue
        fi
        
        if [[ "$actual" == "${CHECKSUMS[$file]}" ]]; then
            echo "ok"
        else
            echo "FAILED!"
            echo "  Expected: ${CHECKSUMS[$file]}"
            echo "  Got:      $actual"
            echo "  The model file may have been updated upstream."
            echo "  If this is expected, update the checksum in this script."
            exit 1
        fi
    fi
done

echo

# Optimize tokenizer.json - remove fixed padding config
# This is critical for correct embeddings (see download.rs comments)
echo "Optimizing tokenizer.json..."
TOKENIZER_PATH="$MODEL_DIR/tokenizer.json"

if command -v jq &> /dev/null; then
    if jq -e '.padding' "$TOKENIZER_PATH" > /dev/null 2>&1; then
        echo "  Removing fixed padding configuration..."
        jq 'del(.padding)' "$TOKENIZER_PATH" > "$TOKENIZER_PATH.tmp"
        mv "$TOKENIZER_PATH.tmp" "$TOKENIZER_PATH"
        echo "✓ Tokenizer optimized (padding config removed)"
    else
        echo "✓ Tokenizer already optimized"
    fi
else
    echo "⚠ jq not installed - cannot optimize tokenizer.json"
    echo "  Install jq and re-run, or manually remove the 'padding' key from tokenizer.json"
    echo "  This is required for correct embedding behavior!"
    exit 1
fi

echo
echo "✓ Model download complete!"
echo "  Location: $MODEL_DIR"
echo
echo "To build with embedded model:"
echo "  cargo build --features embedded-model --no-default-features"
