/**
 * Standalone test to check tokenization
 */

use std::fs;
use tokenizers::Tokenizer;

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    let tokenizer_path = "models/all-MiniLM-L6-v2/tokenizer.json";
    let tokenizer_json = fs::read_to_string(tokenizer_path)?;
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
    println!("\nThese should match Python's AutoTokenizer output");

    Ok(())
}
