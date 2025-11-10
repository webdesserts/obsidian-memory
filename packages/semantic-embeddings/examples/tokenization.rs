//! Tokenization verification example
//!
//! **Purpose:** Verify that tokenization is working correctly and matches expected BERT format
//!
//! **When to use:**
//! - Debugging tokenization issues
//! - Verifying token IDs match expected BERT vocabulary
//! - Checking attention mask and special token placement ([CLS], [SEP])
//!
//! **Usage:**
//! ```bash
//! cargo run --example tokenization
//! ```
//!
//! **Expected output:**
//! - Token IDs starting with 101 ([CLS]) and ending with 102 ([SEP])
//! - Attention mask with 1s for real tokens
//! - Decoded tokens showing word pieces

use std::fs;
use std::path::PathBuf;
use tokenizers::Tokenizer;

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Load tokenizer using same path resolution as ModelManager
    let model_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("models/all-MiniLM-L6-v2");

    let tokenizer_json = fs::read_to_string(model_dir.join("tokenizer.json"))?;
    let tokenizer = Tokenizer::from_bytes(tokenizer_json.as_bytes())?;

    let texts = vec![
        "The weather is lovely today",
        "He drove to the stadium",
        "The new movie is awesome",
        "The new movie is so great",
    ];

    println!("Tokenization Analysis\n");
    println!("{}", "=".repeat(80));

    for text in texts {
        let encoding = tokenizer.encode(text, true)?;
        let ids = encoding.get_ids();
        let tokens = encoding.get_tokens();
        let attention_mask = encoding.get_attention_mask();

        println!("\nText: \"{}\"", text);
        println!("Token count: {}", ids.len());
        println!("IDs:       {:?}", ids);
        println!("Tokens:    {:?}", tokens);
        println!("Attention: {:?}", attention_mask);
    }

    println!("\n{}", "=".repeat(80));
    println!("\nExpected:");
    println!("- Token IDs should start with 101 ([CLS]) and end with 102 ([SEP])");
    println!("- Attention mask: 1 for real tokens, 0 for padding");
    println!("- Should match Python's AutoTokenizer output");

    Ok(())
}
