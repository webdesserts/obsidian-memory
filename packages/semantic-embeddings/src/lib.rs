#![deny(clippy::all)]

use wasm_bindgen::prelude::*;
use js_sys::{Array, Float32Array};

mod model;
mod embedding;

use model::ModelManager;
use std::sync::Arc;

// Set panic hook for better error messages in WASM
#[wasm_bindgen(start)]
pub fn init() {
    console_error_panic_hook::set_once();
}

/// Semantic embedding generator for text content
#[wasm_bindgen]
pub struct SemanticEmbeddings {
    model: Arc<ModelManager>,
}

#[wasm_bindgen]
impl SemanticEmbeddings {
    /// Create a new SemanticEmbeddings instance
    ///
    /// Call loadModel() to initialize with model data before using encode()
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        Self {
            model: Arc::new(ModelManager::new()),
        }
    }

    /// Load model from provided data
    ///
    /// Must be called before encode() or encodeBatch()
    ///
    /// # Arguments
    /// * `config_json` - JSON string containing model config
    /// * `tokenizer_json` - JSON string containing tokenizer config
    /// * `model_weights` - Uint8Array containing model weights (safetensors format)
    #[wasm_bindgen(js_name = loadModel)]
    pub fn load_model(
        &self,
        config_json: String,
        tokenizer_json: String,
        model_weights: &[u8],
    ) -> Result<(), JsValue> {
        self.model.load_model(&config_json, &tokenizer_json, model_weights)
            .map_err(|e| JsValue::from_str(&format!("Failed to load model: {}", e)))
    }

    /// Encode a single text into an embedding vector
    ///
    /// Returns a Float32Array of dimension 384 (for all-MiniLM-L6-v2)
    #[wasm_bindgen]
    pub fn encode(&self, text: String) -> Result<Float32Array, JsValue> {
        let embedding = self.model.encode_single(&text)
            .map_err(|e| JsValue::from_str(&format!("Embedding error: {}", e)))?;

        // Convert Vec<f32> to Float32Array
        let array = Float32Array::new_with_length(embedding.len() as u32);
        array.copy_from(&embedding);
        Ok(array)
    }

    /// Encode multiple texts in batch (more efficient than multiple encode() calls)
    ///
    /// Returns array of Float32Arrays
    #[wasm_bindgen(js_name = encodeBatch)]
    pub fn encode_batch(&self, texts: Vec<JsValue>) -> Result<Array, JsValue> {
        // Convert JsValues to Strings
        let text_strings: Vec<String> = texts.into_iter()
            .map(|v| v.as_string().unwrap_or_default())
            .collect();

        let embeddings = self.model.encode_batch(&text_strings)
            .map_err(|e| JsValue::from_str(&format!("Embedding error: {}", e)))?;

        // Convert Vec<Vec<f32>> to Array of Float32Arrays
        let result = Array::new();
        for embedding in embeddings {
            let array = Float32Array::new_with_length(embedding.len() as u32);
            array.copy_from(&embedding);
            result.push(&array);
        }
        Ok(result)
    }

    /// Compute cosine similarity between two embedding vectors
    ///
    /// Returns a value between -1 and 1 (typically 0 to 1 for similar texts)
    #[wasm_bindgen(js_name = cosineSimilarity)]
    pub fn cosine_similarity(&self, a: Float32Array, b: Float32Array) -> Result<f32, JsValue> {
        let a_vec: Vec<f32> = a.to_vec();
        let b_vec: Vec<f32> = b.to_vec();

        if a_vec.len() != b_vec.len() {
            return Err(JsValue::from_str(&format!(
                "Vector dimensions must match: {} vs {}", a_vec.len(), b_vec.len()
            )));
        }

        embedding::cosine_similarity(&a_vec, &b_vec)
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }

    /// Find indices of top K most similar embeddings to query
    ///
    /// Returns array of indices sorted by similarity (descending)
    #[wasm_bindgen(js_name = findMostSimilar)]
    pub fn find_most_similar(
        &self,
        query: Float32Array,
        candidates: Array,
        top_k: u32,
    ) -> Result<Vec<u32>, JsValue> {
        let query_vec: Vec<f32> = query.to_vec();
        let candidate_vecs: Vec<Vec<f32>> = (0..candidates.length())
            .filter_map(|i| {
                candidates.get(i).dyn_into::<Float32Array>().ok()
                    .map(|arr| arr.to_vec())
            })
            .collect();

        embedding::find_most_similar(&query_vec, &candidate_vecs, top_k as usize)
            .map_err(|e| JsValue::from_str(&e.to_string()))
    }
}
