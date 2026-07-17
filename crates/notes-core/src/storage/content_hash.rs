//! Content hash utilities for optimistic locking.
//!
//! Provides content hashing for verifying files haven't changed between read and write.
//! Clients receive a content_hash when reading a note and must provide it when writing
//! to prove they've seen the current content.

use sha2::{Digest, Sha256};

/// A content hash representing file state at time of read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ContentHash(String);

impl ContentHash {
    /// Compute hash from content bytes.
    pub fn from_content(content: &str) -> Self {
        let mut hasher = Sha256::new();
        hasher.update(content.as_bytes());
        let result = hasher.finalize();
        Self(hex::encode(result))
    }

    /// Get the hash string for use with Storage::write() optimistic locking.
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_content_hash_deterministic() {
        let hash1 = ContentHash::from_content("hello world");
        let hash2 = ContentHash::from_content("hello world");
        assert_eq!(hash1, hash2);
    }

    #[test]
    fn test_different_content_different_hash() {
        let hash1 = ContentHash::from_content("hello");
        let hash2 = ContentHash::from_content("world");
        assert_ne!(hash1, hash2);
    }

    #[test]
    fn test_hash_as_str() {
        let hash = ContentHash::from_content("test");
        // SHA-256 hex is 64 characters
        assert_eq!(hash.as_str().len(), 64);
    }
}
