# Embedding Similarity Investigation

## Problem Statement

Our Rust/WASM embedding implementation produces systematically higher similarity scores than the official Python sentence-transformers library, with a particularly large discrepancy for low-similarity pairs.

## Comprehensive Test Results (9 test cases)

Using model: `sentence-transformers/all-MiniLM-L6-v2`
Tolerance: ±0.05 (5%)
**Result: 1/9 tests passed**

### Passing Tests ✓
1. **medium_similarity_weather** ✓
   - "The weather is lovely today" vs "It's so sunny outside!"
   - Expected: 0.6660 | Actual: 0.6871 | Error: +3.2%

### Failing Tests ✗

#### High Similarity Tests (should be 0.75-0.95)
2. **high_similarity_movie** ✗
   - "The new movie is awesome" vs "The new movie is so great"
   - Expected: 0.8939 | Actual: 0.9489 | Error: +6.2%

3. **high_similarity_paraphrase_1** ✗
   - "A man is eating food" vs "A man is eating a piece of bread"
   - Expected: 0.7500 | Actual: 0.8482 | Error: +13.1%

4. **very_high_similarity_identical_meaning** ✗
   - "The cat sat on the mat" vs "A cat was sitting on the mat"
   - Expected: 0.8500 | Actual: 0.9135 | Error: +7.5%

#### Medium Similarity Tests (should be 0.60-0.75)
5. **medium_similarity_related** ✗
   - "A woman is playing violin" vs "A man is playing guitar"
   - Expected: 0.6500 | Actual: 0.7258 | Error: +11.7%

#### Low Similarity Tests (should be 0.05-0.15) ⚠️ CRITICAL
6. **low_similarity_weather_stadium** ✗
   - "The weather is lovely today" vs "He drove to the stadium"
   - Expected: 0.1046 | Actual: 0.4576 | Error: **+337.5%**

7. **low_similarity_sunny_stadium** ✗
   - "It's so sunny outside!" vs "He drove to the stadium"
   - Expected: 0.1411 | Actual: 0.4388 | Error: **+211.0%**

8. **low_similarity_unrelated_1** ✗
   - "A man is eating food" vs "A plane is taking off"
   - Expected: 0.1000 | Actual: 0.4559 | Error: **+355.9%**

9. **low_similarity_different_topics** ✗
   - "How to bake a cake" vs "Installing Python packages"
   - Expected: 0.0500 | Actual: 0.3494 | Error: **+598.9%**

## Error Pattern Analysis

### High Similarity (3 tests)
- Error range: +6.2% to +13.1%
- Average error: +8.9%
- Pattern: Consistently scoring ~5-10% higher

### Medium Similarity (2 tests)
- Error range: +3.2% to +11.7%
- Average error: +7.5%
- Pattern: One excellent match, one slightly high

### Low Similarity (4 tests) ⚠️
- Error range: +211% to +599%
- Average error: +375.8%
- Pattern: **Catastrophic failure** - unrelated content scores 35-46% similar when it should be 5-15%

**Critical Finding:** The model cannot distinguish unrelated content. Everything scores at least 35% similar, making search results useless.

## What We've Verified

✅ **Tokenization**: Token IDs are correct (verified with `examples/tokenization.rs`)
  - Example: "The weather is lovely today" → `[101, 1996, 4633, 2003, 8403, 2651, 102, ...]`
  - Proper [CLS] and [SEP] tokens added
  - Padding handled correctly

✅ **Attention Masks**: Using proper tokenizer attention masks (not all-1s)
  - Real tokens: 1
  - Padding tokens: 0
  - This was a bug we fixed earlier

✅ **Mean Pooling**: Implementation matches Python exactly
  - Expand mask to [batch, seq, hidden] via `broadcast_as()`
  - Convert to F32 for computation
  - Divide by clamped mask sum (min 1e-9)

✅ **L2 Normalization**: All embeddings properly normalized
  - L2 norm ≈ 1.0 for all outputs
  - Makes cosine similarity equivalent to dot product

✅ **Cosine Similarity**: Computation is correct
  - Tested with identity vectors (1.0)
  - Tested with orthogonal vectors (0.0)
  - Tested with manual calculation

## Embedding Statistics Analysis

### Low Similarity Pair ("weather" vs "stadium")

**Embedding 1 (weather):**
- Mean: 0.001845, Std Dev: 0.050998
- Min/Max: -0.1305 / 0.1567
- L2 Norm: 1.000000
- 185 positive, 199 negative, 65 near-zero dimensions

**Embedding 2 (stadium):**
- Mean: 0.001671, Std Dev: 0.051004
- Min/Max: -0.1396 / 0.1721
- L2 Norm: 1.000000
- 191 positive, 193 negative, 74 near-zero dimensions

**Overlap Analysis:**
- Same sign: 258/384 dimensions (67.2%)
- Mean element-wise diff: 0.040761
- Max element-wise diff: 0.225314

### High Similarity Pair ("movie awesome" vs "movie great")

**Overlap Analysis:**
- Same sign: 337/384 dimensions (87.8%)
- Mean element-wise diff: 0.012759
- Max element-wise diff: 0.060820

## Root Cause Analysis

The issue is NOT in our pooling, normalization, or similarity computation. All of those are verified correct.

The issue appears to be in **Candle's BertModel.forward() implementation** producing different intermediate values than PyTorch's implementation.

### Possible Causes

1. **Layer Normalization**: Candle vs PyTorch may handle epsilon or precision differently
2. **Attention Computation**: Subtle differences in softmax or masking
3. **Floating Point Precision**: f32 accumulation vs PyTorch's mixed precision
4. **Activation Functions**: GELU or other activations may differ slightly
5. **Model Weight Loading**: Weights may not be loading identically from safetensors

## Impact on Search Functionality

### Current Behavior
With our implementation, unrelated notes (should be ~10% similar) score ~45% similar.

### Potential Issues
- **False positives**: Unrelated notes may appear in search results
- **Poor ranking**: Truly relevant notes may not stand out from noise
- **User confusion**: Search returns too many marginally-related results

### Example Scenario
Query: "MCP Servers"
- Note about MCP Servers: ~86% similar ✓
- Note about Telemark skiing: ~88% similar ✗ (should be ~20%)

This is NOT acceptable for production use.

## Investigation Progress

### What We've Ruled Out ✓

1. **Tokenization** - Verified token IDs match expected BERT format exactly
2. **Attention Masks** - Using proper tokenizer masks (not all-1s), bug previously fixed
3. **Mean Pooling** - Implementation matches Python exactly (broadcast + clamp)
4. **L2 Normalization** - All embeddings properly normalized (norm = 1.0)
5. **Cosine Similarity** - Computation verified with manual tests
6. **GELU Activation** - Both Candle and PyTorch use exact GELU (erf) by default
7. **Model Config** - Config matches expected BERT parameters

### Observations from Debug Tests

**Embeddings Statistics (Low Similarity Pair):**
- Norms: Perfect (1.000000)
- Sparsity: Reasonable (~17-19% near-zero)
- Means: Close to zero (~0.0018)

**Sign Pattern Analysis:**
- High similarity: 87.8% same sign (expected: ~85-95%)
- Low similarity: 67.2% same sign (expected: ~45-55%)
- **Issue**: 67.2% is too high for truly unrelated content

This 17% bias (67.2% - 50%) in same-sign dimensions for unrelated content directly correlates to the elevated similarity scores we're seeing.

### Hypotheses to Test

1. **Layer Normalization Epsilon**
   - Candle vs PyTorch may handle epsilon differently
   - Config specifies `layer_norm_eps: 1e-12`
   - Small differences compound through 6 layers

2. **Attention Mechanism Implementation**
   - Softmax precision
   - Attention score computation
   - Dropout handling during inference

3. **Model Weight Precision**
   - safetensors loading may introduce subtle precision differences
   - f32 vs f64 in critical operations

4. **Unknown Candle Implementation Detail**
   - Some BERT operation differs subtly from PyTorch
   - Would explain consistent bias across all tests

## Next Steps

### Immediate: Create Python Baseline
1. Generate embeddings for all test cases using Python sentence-transformers
2. Save raw embeddings to JSON for direct comparison
3. Compare our embeddings element-by-element to find patterns

### Deep Dive: Model Forward Pass
1. Add comprehensive debug logging to capture:
   - Raw BERT output before pooling
   - Intermediate layer outputs
   - Attention weights
2. Create matching Python script with same debug points
3. Binary search for where outputs diverge

### Alternative Approaches
1. **Try ONNX Runtime**
   - Export sentence-transformers model to ONNX
   - Use ort-rs (ONNX Runtime for Rust)
   - Guaranteed to match Python exactly

2. **Use PyO3 + sentence-transformers directly**
   - Call Python library from Rust
   - Trade-off: Python dependency but guaranteed correctness
   - Benchmark performance impact

3. **Switch to different Rust ML framework**
   - Try burn-rs or tract
   - See if issue is Candle-specific

## Test Infrastructure

### Fixtures
- `fixtures/similarity-reference.toml` - 9 test cases with reference similarity scores from official Python implementation
  - Uses TOML format for easy commenting and readability
  - Includes inline documentation for each test case category

### Tests (in `tests/` directory)
- `tests/common/mod.rs` - Shared test utilities (model loading, fixtures, cosine similarity)
- `tests/similarity_fixtures.rs` - Main fixture-based validation (1/9 tests passing)
- `tests/basic_functionality.rs` - Basic feature tests (padding, batch encoding, regression tests)
- `tests/debug_embeddings.rs` - Debug tests for analyzing embedding properties (run with `--ignored`)

### Examples
- `examples/tokenization.rs` - Standalone tokenization verification tool

## References

- Official similarity examples: https://sbert.net/docs/sentence_transformer/usage/semantic_textual_similarity.html
- Sentence transformers docs: https://www.sbert.net/
- Candle transformers: https://github.com/huggingface/candle
- Model card: https://huggingface.co/sentence-transformers/all-MiniLM-L6-v2
