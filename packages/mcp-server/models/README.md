# Models Directory

This directory stores GGUF quantized models for local LLM inference used by the Search tool.

## Required Model

**DeepSeek-R1-Distill-Qwen-1.5B Q4_K_M**
- Size: ~1.12 GB
- Use case: Graph search decision-making with reasoning capabilities
- MATH-500 benchmark: 83.9%

## Download Instructions

### Option A: Using npx (Recommended)

```bash
cd packages/mcp-server
npx node-llama-cpp pull \
  --repo bartowski/DeepSeek-R1-Distill-Qwen-1.5B-GGUF \
  --file DeepSeek-R1-Distill-Qwen-1.5B-Q4_K_M.gguf \
  --dir ./models
```

### Option B: Using Hugging Face CLI

```bash
pip install huggingface-cli
huggingface-cli download \
  bartowski/DeepSeek-R1-Distill-Qwen-1.5B-GGUF \
  --include "DeepSeek-R1-Distill-Qwen-1.5B-Q4_K_M.gguf" \
  --local-dir ./packages/mcp-server/models/
```

### Option C: Manual Download

Visit: https://huggingface.co/bartowski/DeepSeek-R1-Distill-Qwen-1.5B-GGUF

Download `DeepSeek-R1-Distill-Qwen-1.5B-Q4_K_M.gguf` and place it in this directory.

## Verification

After downloading, verify the model exists:

```bash
ls -lh models/DeepSeek-R1-Distill-Qwen-1.5B-Q4_K_M.gguf
```

Expected size: ~1.12 GB

## Performance Expectations

- **First load**: 2-5 seconds
- **Token generation**: 10-30 tokens/sec (CPU), 50-100 tokens/sec (Metal GPU)
- **Memory usage**: ~1.7-2.2 GB RAM total
- **Typical search decision**: 100-200 tokens = 3-10 seconds

## Why This Model?

DeepSeek-R1-Distill-Qwen-1.5B combines:
- Small size (1.12 GB) for fast loading
- Strong reasoning capabilities (distilled from 671B parameter R1 model)
- Excellent at structured JSON output
- Performs well on decision-making tasks

See `knowledge/DeepSeek R1.md` and `knowledge/Local LLM Integration.md` for more details.
