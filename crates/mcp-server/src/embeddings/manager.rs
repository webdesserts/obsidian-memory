//! Embedding manager for generating and caching note embeddings.

use anyhow::{Context, Result};
use semantic_embeddings::SemanticEmbeddings;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use tokio::fs;
use tokio::sync::RwLock;

use super::download::download_model;

/// Cache entry storing an embedding and its content hash.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct CacheEntry {
    /// SHA-256 hash of the note content
    content_hash: String,
    /// The embedding vector
    embedding: Vec<f32>,
}

/// Manages semantic embeddings for notes.
///
/// Handles model loading, embedding generation, and caching.
pub struct EmbeddingManager {
    /// The embedding model
    embeddings: Arc<SemanticEmbeddings>,
    /// Cache of note embeddings: note_path -> (content_hash, embedding)
    cache: RwLock<HashMap<String, CacheEntry>>,
    /// Path to the cache file
    cache_path: PathBuf,
    /// Whether the model is loaded
    model_loaded: RwLock<bool>,
    /// Path to the model directory
    model_dir: PathBuf,
}

impl EmbeddingManager {
    /// Create a new embedding manager.
    ///
    /// The model will be downloaded automatically if not present.
    pub fn new(vault_path: &Path) -> Self {
        let model_dir = vault_path.join(".obsidian/models/all-MiniLM-L6-v2");
        let cache_path = vault_path.join(".obsidian/embedding-cache.json");

        Self {
            embeddings: Arc::new(SemanticEmbeddings::new()),
            cache: RwLock::new(HashMap::new()),
            cache_path,
            model_loaded: RwLock::new(false),
            model_dir,
        }
    }

    /// Initialize the embedding manager by loading the model.
    ///
    /// Downloads the model if not present. Uses write lock for the entire
    /// operation to prevent race conditions with concurrent initialization.
    pub async fn initialize(&self) -> Result<()> {
        // Hold write lock for entire initialization to prevent TOCTOU race
        let mut loaded = self.model_loaded.write().await;
        if *loaded {
            return Ok(());
        }

        // Download model if needed
        download_model(&self.model_dir).await?;

        // Load model from disk
        self.embeddings
            .load_model_from_dir(&self.model_dir)
            .context("Failed to load embedding model")?;

        *loaded = true;

        // Load cache from disk
        self.load_cache().await?;

        tracing::info!("Embedding manager initialized");
        Ok(())
    }

    /// Ensure the model is loaded before use.
    async fn ensure_loaded(&self) -> Result<()> {
        if !*self.model_loaded.read().await {
            self.initialize().await?;
        }
        Ok(())
    }

    /// Get or compute embedding for a note.
    ///
    /// Uses cached embedding if content hasn't changed.
    pub async fn get_embedding(&self, note_path: &str, content: &str) -> Result<Vec<f32>> {
        self.ensure_loaded().await?;

        let content_hash = compute_hash(content);

        // Check cache
        {
            let cache = self.cache.read().await;
            if let Some(entry) = cache.get(note_path) {
                if entry.content_hash == content_hash {
                    return Ok(entry.embedding.clone());
                }
            }
        }

        // Compute new embedding
        let embedding = self.embeddings.encode(content)?;

        // Update cache
        {
            let mut cache = self.cache.write().await;
            cache.insert(
                note_path.to_string(),
                CacheEntry {
                    content_hash,
                    embedding: embedding.clone(),
                },
            );
        }

        Ok(embedding)
    }

    /// Get embeddings for multiple notes in batch.
    pub async fn get_embeddings_batch(
        &self,
        notes: &[(String, String)], // (path, content)
    ) -> Result<Vec<(String, Vec<f32>)>> {
        self.ensure_loaded().await?;

        let mut results = Vec::with_capacity(notes.len());
        let mut to_compute: Vec<(usize, String, String)> = Vec::new();

        // Check cache for each note
        {
            let cache = self.cache.read().await;
            for (idx, (path, content)) in notes.iter().enumerate() {
                let content_hash = compute_hash(content);

                if let Some(entry) = cache.get(path) {
                    if entry.content_hash == content_hash {
                        results.push((path.clone(), entry.embedding.clone()));
                        continue;
                    }
                }

                // Need to compute this one
                to_compute.push((idx, path.clone(), content.clone()));
            }
        }

        // Batch compute embeddings for cache misses
        if !to_compute.is_empty() {
            let texts: Vec<String> = to_compute.iter().map(|(_, _, c)| c.clone()).collect();
            let embeddings = self.embeddings.encode_batch(&texts)?;

            let mut cache = self.cache.write().await;
            for ((_, path, content), embedding) in to_compute.into_iter().zip(embeddings) {
                let content_hash = compute_hash(&content);
                cache.insert(
                    path.clone(),
                    CacheEntry {
                        content_hash,
                        embedding: embedding.clone(),
                    },
                );
                results.push((path, embedding));
            }
        }

        Ok(results)
    }

    /// Save cache to disk.
    pub async fn save_cache(&self) -> Result<()> {
        let cache = self.cache.read().await;
        let json = serde_json::to_string(&*cache)?;

        // Ensure parent directory exists
        if let Some(parent) = self.cache_path.parent() {
            fs::create_dir_all(parent).await?;
        }

        fs::write(&self.cache_path, json).await?;
        tracing::debug!("Saved embedding cache ({} entries)", cache.len());
        Ok(())
    }

    /// Load cache from disk.
    async fn load_cache(&self) -> Result<()> {
        if !self.cache_path.exists() {
            return Ok(());
        }

        let json = fs::read_to_string(&self.cache_path).await?;
        
        // Try to load cache, but if format is incompatible (old cache from TypeScript),
        // just start fresh rather than failing
        match serde_json::from_str::<HashMap<String, CacheEntry>>(&json) {
            Ok(loaded) => {
                let mut cache = self.cache.write().await;
                *cache = loaded;
                tracing::debug!("Loaded embedding cache ({} entries)", cache.len());
            }
            Err(e) => {
                tracing::warn!(
                    "Failed to load embedding cache (format incompatible): {}. Starting with empty cache.",
                    e
                );
                // Delete the incompatible cache file
                if let Err(del_err) = fs::remove_file(&self.cache_path).await {
                    tracing::warn!("Failed to delete incompatible cache: {}", del_err);
                }
            }
        }

        Ok(())
    }

    /// Invalidate cache entry for a note.
    pub async fn invalidate(&self, note_path: &str) {
        let mut cache = self.cache.write().await;
        cache.remove(note_path);
    }

    /// Compute cosine similarity between two embeddings.
    pub fn cosine_similarity(a: &[f32], b: &[f32]) -> Result<f32> {
        SemanticEmbeddings::cosine_similarity(a, b)
    }

    /// Find most similar notes to a query embedding.
    pub fn find_most_similar(
        query: &[f32],
        candidates: &[Vec<f32>],
        top_k: usize,
    ) -> Result<Vec<u32>> {
        SemanticEmbeddings::find_most_similar(query, candidates, top_k)
    }
}

/// Compute SHA-256 hash of content.
fn compute_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.as_bytes());
    hex::encode(hasher.finalize())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_compute_hash() {
        let hash1 = compute_hash("hello world");
        let hash2 = compute_hash("hello world");
        let hash3 = compute_hash("different content");

        assert_eq!(hash1, hash2);
        assert_ne!(hash1, hash3);
        assert_eq!(hash1.len(), 64); // SHA-256 hex = 64 chars
    }
}
