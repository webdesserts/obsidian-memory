use semantic_embeddings::ModelManager;
use std::fs;
use std::path::PathBuf;

fn main() -> Result<(), Box<dyn std::error::Error + Send + Sync>> {
    // Logging is auto-initialized when --features debug is used
    let manager = ModelManager::new();

    // Load model files
    let model_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("models/all-MiniLM-L6-v2");

    let config = fs::read_to_string(model_dir.join("config.json"))?;
    let tokenizer = fs::read_to_string(model_dir.join("tokenizer.json"))?;
    let weights = fs::read(model_dir.join("model.safetensors"))?;

    manager.load_model(&config, &tokenizer, &weights)?;

    println!("\n=== Rust BERT Output Debug ===");
    println!("Text: 'cat'");

    // Encode the same text as Python
    let _embedding = manager.encode_single("cat")?;

    println!();

    Ok(())
}
