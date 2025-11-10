//! Debug raw BERT model output
//!
//! **Purpose:** Inspect raw BERT embeddings before pooling and normalization
//!
//! **When to use:**
//! - Comparing Rust/Candle BERT output against Python/PyTorch
//! - Investigating where embeddings diverge from reference implementation
//! - Verifying model weights loaded correctly
//!
//! **Usage:**
//! ```bash
//! # With debug logging enabled
//! cargo run --example debug-bert-output --features debug
//!
//! # Without debug logging
//! cargo run --example debug-bert-output
//! ```
//!
//! **Expected output:**
//! - When `--features debug` is enabled: Detailed tensor shapes and values from ModelManager
//! - Without debug: Just the embedding dimensions
//!
//! **Note:** This example uses ModelManager from src/, so any changes to model loading
//! or inference automatically apply here (no implementation drift).

use semantic_embeddings::ModelManager;
use std::fs;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Create ModelManager (uses same implementation as production code)
    let manager = ModelManager::new();

    // Load model files using same path resolution as tests
    let model_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("models/all-MiniLM-L6-v2");

    let config = fs::read_to_string(model_dir.join("config.json"))?;
    let tokenizer = fs::read_to_string(model_dir.join("tokenizer.json"))?;
    let weights = fs::read(model_dir.join("model.safetensors"))?;

    manager.load_model(&config, &tokenizer, &weights)?;

    println!("\n=== BERT Output Debug ===");
    println!("Text: 'cat'");
    println!();

    // Encode the text - debug logging (if enabled) will show internal details
    let embedding = manager.encode_single("cat")?;

    println!("Generated embedding:");
    println!("  Dimensions: {}", embedding.len());
    println!("  First 10 values: {:?}", &embedding[..10]);
    println!();

    #[cfg(feature = "debug")]
    {
        println!("✓ Debug logging enabled - see detailed output above");
        println!("  (token counts, attention masks, raw BERT output, pooling steps)");
    }

    #[cfg(not(feature = "debug"))]
    {
        println!("ℹ For detailed debug output, run with:");
        println!("  cargo run --example debug-bert-output --features debug");
    }

    println!();
    println!("To compare against Python PyTorch implementation:");
    println!("  python debug-bert-output.py");

    Ok(())
}
