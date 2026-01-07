//! Semantic embeddings management for the MCP server.
//!
//! This module handles:
//! - Model downloading from Hugging Face (with `download-model` feature)
//! - Model loading from embedded binary (with `embedded-model` feature)
//! - Embedding generation with caching
//! - Cache persistence to disk

#[cfg(feature = "download-model")]
mod download;
mod manager;

pub use manager::EmbeddingManager;
