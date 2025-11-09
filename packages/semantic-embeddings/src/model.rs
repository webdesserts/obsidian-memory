use anyhow::{Context, Result};
use candle_core::{Device, Tensor};
use candle_nn::VarBuilder;
use candle_transformers::models::bert::{BertModel, Config};
use std::sync::Mutex;
use tokenizers::Tokenizer;

/// Model manager handles loading and inference with the sentence transformer model
pub struct ModelManager {
    state: Mutex<Option<ModelState>>,
}

struct ModelState {
    model: BertModel,
    tokenizer: Tokenizer,
    device: Device,
}

impl ModelManager {
    pub fn new() -> Self {
        Self {
            state: Mutex::new(None),
        }
    }

    /// Initialize the model with data provided from JavaScript
    ///
    /// # Arguments
    /// * `config_json` - JSON string containing model config
    /// * `tokenizer_json` - JSON string containing tokenizer config
    /// * `model_weights` - Byte array containing model weights (safetensors format)
    pub fn load_model(
        &self,
        config_json: &str,
        tokenizer_json: &str,
        model_weights: &[u8],
    ) -> Result<()> {
        let mut state_guard = self.state.lock().unwrap();

        if state_guard.is_some() {
            return Ok(()); // Already loaded
        }

        eprintln!("[Embeddings] Loading model from provided data...");

        // Use CPU for now (Metal/CUDA support can be added later)
        let device = Device::Cpu;

        // Load tokenizer from JSON string
        let tokenizer = Tokenizer::from_bytes(tokenizer_json.as_bytes())
            .map_err(|e| anyhow::anyhow!("Failed to load tokenizer: {}", e))?;

        // Parse model config
        let config: Config = serde_json::from_str(config_json)
            .context("Failed to parse config.json")?;

        // Load model weights from bytes
        let vb = VarBuilder::from_buffered_safetensors(
            model_weights.to_vec(),
            candle_core::DType::F32,
            &device,
        )?;

        let model = BertModel::load(vb, &config)?;

        eprintln!("[Embeddings] Model loaded successfully");

        *state_guard = Some(ModelState {
            model,
            tokenizer,
            device,
        });

        Ok(())
    }

    /// Check if model is loaded
    fn ensure_loaded(&self) -> Result<()> {
        let state_guard = self.state.lock().unwrap();

        if state_guard.is_none() {
            anyhow::bail!(
                "Model not loaded. Call loadModel() first with model data."
            );
        }

        Ok(())
    }

    /// Encode a single text into embedding vector
    pub fn encode_single(&self, text: &str) -> Result<Vec<f32>> {
        self.ensure_loaded()?;

        let state_guard = self.state.lock().unwrap();
        let state = state_guard.as_ref().unwrap();

        // Tokenize
        let encoding = state.tokenizer
            .encode(text, true)
            .map_err(|e| anyhow::anyhow!("Tokenization failed: {}", e))?;

        let tokens: Vec<u32> = encoding.get_ids().to_vec();
        let token_ids = Tensor::new(&tokens[..], &state.device)?
            .unsqueeze(0)?; // Add batch dimension

        // Create attention mask (all 1s for single sequence)
        let attention_mask = Tensor::ones(
            (1, tokens.len()),
            candle_core::DType::U32,
            &state.device,
        )?;

        // Run model inference (token_type_ids = None for single sequence)
        let output = state.model.forward(&token_ids, &attention_mask, None)?;

        // Convert attention mask to F32 for mean pooling
        let attention_mask_f32 = attention_mask.to_dtype(candle_core::DType::F32)?;

        // Mean pooling over sequence dimension
        let embedding = self.mean_pool(&output, &attention_mask_f32)?;

        // Normalize embedding
        let normalized = self.normalize(&embedding)?;

        // Remove batch dimension and convert to Vec<f32>
        normalized.squeeze(0)?
            .to_vec1()
            .context("Failed to convert tensor to vec")
    }

    /// Encode multiple texts in batch
    pub fn encode_batch(&self, texts: &[String]) -> Result<Vec<Vec<f32>>> {
        if texts.is_empty() {
            return Ok(vec![]);
        }

        self.ensure_loaded()?;

        let state_guard = self.state.lock().unwrap();
        let state = state_guard.as_ref().unwrap();

        // Encode all texts
        let encodings: Vec<_> = texts
            .iter()
            .map(|text| {
                state.tokenizer
                    .encode(text.as_str(), true)
                    .map_err(|e| anyhow::anyhow!("Tokenization failed: {}", e))
            })
            .collect::<Result<Vec<_>>>()?;

        // Find max length for padding
        let max_len = encodings.iter().map(|e| e.len()).max().unwrap_or(0);

        // Pad all sequences to same length and create batch tensors
        let mut all_token_ids = Vec::new();
        let mut all_masks = Vec::new();

        for encoding in &encodings {
            let tokens: Vec<u32> = encoding.get_ids().to_vec();
            let mut padded_tokens = tokens.clone();
            let mut mask = vec![1u32; tokens.len()];

            // Pad to max_len
            while padded_tokens.len() < max_len {
                padded_tokens.push(0); // PAD token
                mask.push(0);
            }

            all_token_ids.push(padded_tokens);
            all_masks.push(mask);
        }

        // Convert to tensors
        let token_ids = Tensor::new(all_token_ids, &state.device)?;
        let attention_mask = Tensor::new(all_masks, &state.device)?;

        // Run batch inference (token_type_ids = None for single sequence)
        let output = state.model.forward(&token_ids, &attention_mask, None)?;

        // Convert attention mask to F32 for mean pooling
        let attention_mask_f32 = attention_mask.to_dtype(candle_core::DType::F32)?;

        // Mean pooling for each item in batch
        let embeddings = self.mean_pool(&output, &attention_mask_f32)?;

        // Normalize embeddings
        let normalized = self.normalize(&embeddings)?;

        // Convert to Vec<Vec<f32>>
        let embedding_vecs: Vec<Vec<f32>> = (0..texts.len())
            .map(|i| {
                normalized.get(i)
                    .ok()
                    .and_then(|tensor| tensor.to_vec1().ok())
                    .unwrap_or_default()
            })
            .collect();

        Ok(embedding_vecs)
    }

    /// Mean pooling: average token embeddings weighted by attention mask
    ///
    /// Uses mean pooling instead of [CLS] token because it better captures the
    /// overall meaning of the text by averaging information from all tokens.
    /// The attention mask weights ensure padding tokens don't affect the average.
    ///
    /// See: https://www.sbert.net/docs/usage/computing_sentence_embeddings.html
    fn mean_pool(&self, token_embeddings: &Tensor, attention_mask: &Tensor) -> Result<Tensor> {
        // token_embeddings shape: [batch_size, seq_len, hidden_size]
        // attention_mask shape: [batch_size, seq_len]

        // Expand mask to match embeddings: [batch_size, seq_len, 1]
        let mask_expanded = attention_mask.unsqueeze(2)?;

        // Multiply embeddings by mask
        let masked_embeddings = token_embeddings.broadcast_mul(&mask_expanded)?;

        // Sum over sequence dimension
        let sum_embeddings = masked_embeddings.sum(1)?;

        // Sum of mask (how many non-padding tokens)
        let sum_mask = attention_mask.sum(1)?.unsqueeze(1)?;

        // Divide to get mean
        sum_embeddings.broadcast_div(&sum_mask)
            .context("Mean pooling failed")
    }

    /// Normalize embeddings to unit length (L2 normalization)
    ///
    /// Projects embeddings onto the unit hypersphere so cosine similarity becomes
    /// equivalent to dot product. This makes similarity computation faster and ensures
    /// embeddings are comparable regardless of input length.
    ///
    /// See: https://www.sbert.net/docs/usage/computing_sentence_embeddings.html
    fn normalize(&self, embeddings: &Tensor) -> Result<Tensor> {
        // Compute L2 norm for each embedding
        let norm = embeddings
            .sqr()?
            .sum_keepdim(embeddings.dims().len() - 1)?
            .sqrt()?;

        // Divide by norm
        embeddings.broadcast_div(&norm)
            .context("Normalization failed")
    }
}
