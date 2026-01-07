//! Embedded model data for all-MiniLM-L6-v2.
//!
//! These files are baked into the binary at compile time via include_bytes!/include_str!.
//! The tokenizer.json must be pre-optimized (padding config removed) before embedding.
//!
//! Model files are located at `models/all-MiniLM-L6-v2/` relative to this crate.
//! Use `scripts/download-model.sh` to download and prepare the model files.

/// Model configuration JSON
pub static CONFIG_JSON: &str = include_str!("../models/all-MiniLM-L6-v2/config.json");

/// Tokenizer configuration JSON (pre-optimized: padding removed)
pub static TOKENIZER_JSON: &str = include_str!("../models/all-MiniLM-L6-v2/tokenizer.json");

/// Model weights (safetensors format, ~90MB)
pub static MODEL_WEIGHTS: &[u8] = include_bytes!("../models/all-MiniLM-L6-v2/model.safetensors");
