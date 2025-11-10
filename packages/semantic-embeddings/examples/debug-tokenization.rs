//! Debug tokenization for fixture test cases
//!
//! **Purpose:** Verify tokenization matches Python implementation for specific test fixtures
//!
//! **When to use:**
//! - Investigating why a specific test case is failing
//! - Comparing Rust tokenization against Python reference
//! - Debugging token ID mismatches between implementations
//!
//! **Usage:**
//! ```bash
//! cargo run --example debug-tokenization
//! ```
//!
//! **Expected output:**
//! - Token IDs for each test pair from fixtures
//! - Should match token IDs from Python's AutoTokenizer exactly
//! - Real tokens (non-padding) are highlighted

use std::fs;
use std::path::PathBuf;
use tokenizers::Tokenizer;

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Load tokenizer using same path resolution as ModelManager
    let model_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("models/all-MiniLM-L6-v2");

    let tokenizer_json = fs::read_to_string(model_dir.join("tokenizer.json"))?;
    let tokenizer = Tokenizer::from_bytes(tokenizer_json.as_bytes())
        .map_err(|e| format!("Failed to load tokenizer: {}", e))?;

    // Test cases from fixtures (covering different similarity categories)
    let test_cases = vec![
        ("The new movie is awesome", "The new movie is so great"),  // High similarity
        ("The weather is lovely today", "It's so sunny outside!"),  // Medium similarity
        ("The weather is lovely today", "He drove to the stadium"),  // Low similarity
        ("The cat sits outside", "A man is playing guitar"),  // Low similarity
    ];

    println!("\n=== Tokenization Debug (Fixture Test Cases) ===\n");

    for (i, &(text1, text2)) in test_cases.iter().enumerate() {
        println!("Test Case {}:", i + 1);
        println!("  '{}' vs '{}'", text1, text2);
        println!("{}", "-".repeat(80));

        // Tokenize both texts
        let encoding1 = tokenizer.encode(text1, true)
            .map_err(|e| format!("Tokenization failed: {}", e))?;
        let encoding2 = tokenizer.encode(text2, true)
            .map_err(|e| format!("Tokenization failed: {}", e))?;

        let token_ids1: Vec<u32> = encoding1.get_ids().to_vec();
        let token_ids2: Vec<u32> = encoding2.get_ids().to_vec();

        // Show only non-padding tokens for clarity
        let real_tokens1: Vec<u32> = token_ids1.iter()
            .take_while(|&&id| id != 0)
            .copied()
            .collect();
        let real_tokens2: Vec<u32> = token_ids2.iter()
            .take_while(|&&id| id != 0)
            .copied()
            .collect();

        println!("  Text 1 token IDs: {:?}", real_tokens1);
        let tokens1: Vec<String> = real_tokens1.iter()
            .map(|&id| tokenizer.decode(&[id], false).unwrap_or_default())
            .collect();
        println!("  Text 1 tokens: {:?}", tokens1);
        println!("  Text 1 length: {} real (total: {})", real_tokens1.len(), token_ids1.len());

        println!("\n  Text 2 token IDs: {:?}", real_tokens2);
        let tokens2: Vec<String> = real_tokens2.iter()
            .map(|&id| tokenizer.decode(&[id], false).unwrap_or_default())
            .collect();
        println!("  Text 2 tokens: {:?}", tokens2);
        println!("  Text 2 length: {} real (total: {})", real_tokens2.len(), token_ids2.len());

        println!("\n");
    }

    println!("{}", "=".repeat(80));
    println!("\nTo compare against Python:");
    println!("  python debug-tokenization.py");
    println!("\nToken IDs should match exactly between Rust and Python implementations.");

    Ok(())
}
