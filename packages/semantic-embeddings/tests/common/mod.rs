use once_cell::sync::Lazy;
use semantic_embeddings::ModelManager;
use serde::Deserialize;
use std::fs;
use std::path::PathBuf;

#[derive(Debug, Deserialize)]
pub struct SimilarityReference {
    pub default_tolerance: f32,
    pub test_cases: Vec<TestCase>,
}

#[derive(Debug, Deserialize)]
pub struct TestCase {
    pub name: String,
    pub text1: String,
    pub text2: String,
    pub expected_similarity: f32,
    #[serde(default)]
    pub tolerance: Option<f32>,
    pub category: String,
}

impl TestCase {
    pub fn tolerance(&self, default: f32) -> f32 {
        self.tolerance.unwrap_or(default)
    }
}

// Shared model instance loaded once for all tests (improves test performance)
pub static TEST_MODEL: Lazy<ModelManager> = Lazy::new(|| {
    let manager = ModelManager::new();

    // Load model files from the models directory
    let model_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("models/all-MiniLM-L6-v2");

    let config = fs::read_to_string(model_dir.join("config.json"))
        .expect("Failed to read config.json");
    let tokenizer = fs::read_to_string(model_dir.join("tokenizer.json"))
        .expect("Failed to read tokenizer.json");
    let weights = fs::read(model_dir.join("model.safetensors"))
        .expect("Failed to read model.safetensors");

    manager.load_model(&config, &tokenizer, &weights)
        .expect("Failed to load model");

    manager
});

// Load reference similarity values from fixtures
pub static SIMILARITY_FIXTURES: Lazy<SimilarityReference> = Lazy::new(|| {
    let fixture_path = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures/similarity-reference.toml");

    let fixture_toml = fs::read_to_string(&fixture_path)
        .expect("Failed to read similarity-reference.toml");

    toml::from_str(&fixture_toml)
        .expect("Failed to parse similarity-reference.toml")
});

pub fn cosine_similarity(a: &[f32], b: &[f32]) -> f32 {
    a.iter().zip(b).map(|(x, y)| x * y).sum()
}
