//! Semantic embeddings management for the MCP server.
//!
//! This module handles:
//! - Model downloading from Hugging Face
//! - Model loading and initialization
//! - Embedding generation with caching
//! - Cache persistence to disk

mod download;
mod manager;

pub use manager::EmbeddingManager;
