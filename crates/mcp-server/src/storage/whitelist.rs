//! Read whitelist for tracking which clients can write to which files.
//!
//! Implements "must read before write" semantics using content hashes:
//! - ReadNote stores the content hash the client saw
//! - WriteNote/EditNote verify the file hasn't changed since the client read it
//! - After a successful write, the new content hash is stored
//!
//! This approach handles:
//! - Multiple MCP servers (each checks against actual file content)
//! - Multiple clients on same server (each tracks their own "last seen" hash)
//! - Consecutive edits by same client (hash updated after each write)
//! - External edits (hash mismatch triggers re-read requirement)

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

/// Unique identifier for a client connection.
///
/// For stdio transport, there's a single implicit client.
/// For remote transport, this will be derived from the connection/session.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct ClientId(pub String);

impl ClientId {
    /// The default client ID for stdio transport (single client per process).
    pub fn stdio() -> Self {
        Self("stdio".to_string())
    }
}

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
}

/// Tracks which clients have read which files and what content they saw.
///
/// Uses content hashes to detect external modifications. A client can only write
/// if the current file content matches what they last read.
///
/// Thread-safe via external synchronization (wrapped in RwLock by MemoryServer).
#[derive(Debug, Default)]
pub struct ReadWhitelist {
    /// Map of path -> (client_id -> content_hash they last saw).
    entries: HashMap<PathBuf, HashMap<ClientId, ContentHash>>,
}

impl ReadWhitelist {
    /// Create a new empty whitelist.
    pub fn new() -> Self {
        Self::default()
    }

    /// Record that a client has read a file with specific content.
    ///
    /// The path should be normalized (relative to vault root, no .md extension).
    pub fn mark_read(&mut self, client: ClientId, path: PathBuf, content_hash: ContentHash) {
        self.entries
            .entry(path)
            .or_default()
            .insert(client, content_hash);
    }

    /// Get the content hash a client last saw for a path.
    ///
    /// Returns None if the client hasn't read this file.
    pub fn get_client_hash(&self, client: &ClientId, path: &Path) -> Option<&ContentHash> {
        self.entries
            .get(path)
            .and_then(|clients| clients.get(client))
    }

    /// Check if a client can write to a path by comparing hashes.
    ///
    /// Returns true if the client has read the file and the current content
    /// matches what they saw (i.e., no external modifications).
    pub fn can_write(&self, client: &ClientId, path: &Path, current_hash: &ContentHash) -> bool {
        self.get_client_hash(client, path)
            .map(|stored_hash| stored_hash == current_hash)
            .unwrap_or(false)
    }

    /// Invalidate all entries for a client (called when client disconnects).
    pub fn invalidate_client(&mut self, client: &ClientId) {
        for clients in self.entries.values_mut() {
            clients.remove(client);
        }
        // Clean up empty entries
        self.entries.retain(|_, clients| !clients.is_empty());
    }

    /// Clear all entries (useful for testing).
    #[cfg(test)]
    pub fn clear(&mut self) {
        self.entries.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(content: &str) -> ContentHash {
        ContentHash::from_content(content)
    }

    #[test]
    fn test_mark_read_allows_write_with_matching_hash() {
        let mut whitelist = ReadWhitelist::new();
        let client = ClientId::stdio();
        let path = PathBuf::from("test");
        let content_hash = hash("hello world");

        // Can't write without reading first
        assert!(!whitelist.can_write(&client, &path, &content_hash));

        whitelist.mark_read(client.clone(), path.clone(), content_hash.clone());

        // Can write when hash matches
        assert!(whitelist.can_write(&client, &path, &content_hash));
    }

    #[test]
    fn test_hash_mismatch_blocks_write() {
        let mut whitelist = ReadWhitelist::new();
        let client = ClientId::stdio();
        let path = PathBuf::from("test");
        let original_hash = hash("original content");
        let modified_hash = hash("modified content");

        whitelist.mark_read(client.clone(), path.clone(), original_hash.clone());

        // Can write with original hash
        assert!(whitelist.can_write(&client, &path, &original_hash));

        // Can't write if file was modified externally (hash mismatch)
        assert!(!whitelist.can_write(&client, &path, &modified_hash));
    }

    #[test]
    fn test_multiple_clients_independent_hashes() {
        let mut whitelist = ReadWhitelist::new();
        let client_a = ClientId("client-a".to_string());
        let client_b = ClientId("client-b".to_string());
        let path = PathBuf::from("test");
        let hash_v1 = hash("version 1");
        let hash_v2 = hash("version 2");

        // Client A reads v1
        whitelist.mark_read(client_a.clone(), path.clone(), hash_v1.clone());

        // Client A can write if file is still v1
        assert!(whitelist.can_write(&client_a, &path, &hash_v1));
        // Client B hasn't read yet
        assert!(!whitelist.can_write(&client_b, &path, &hash_v1));

        // Client B reads v1
        whitelist.mark_read(client_b.clone(), path.clone(), hash_v1.clone());

        // Both can write
        assert!(whitelist.can_write(&client_a, &path, &hash_v1));
        assert!(whitelist.can_write(&client_b, &path, &hash_v1));

        // Client A writes, updates their stored hash to v2
        whitelist.mark_read(client_a.clone(), path.clone(), hash_v2.clone());

        // Now file is v2 on disk:
        // - Client A can write (they have v2)
        // - Client B can't write (they still have v1, but file is v2)
        assert!(whitelist.can_write(&client_a, &path, &hash_v2));
        assert!(!whitelist.can_write(&client_b, &path, &hash_v2));
    }

    #[test]
    fn test_invalidate_client() {
        let mut whitelist = ReadWhitelist::new();
        let client_a = ClientId("client-a".to_string());
        let client_b = ClientId("client-b".to_string());
        let path1 = PathBuf::from("test1");
        let path2 = PathBuf::from("test2");
        let content_hash = hash("content");

        whitelist.mark_read(client_a.clone(), path1.clone(), content_hash.clone());
        whitelist.mark_read(client_a.clone(), path2.clone(), content_hash.clone());
        whitelist.mark_read(client_b.clone(), path1.clone(), content_hash.clone());

        whitelist.invalidate_client(&client_a);

        // Client A can no longer write to either path
        assert!(!whitelist.can_write(&client_a, &path1, &content_hash));
        assert!(!whitelist.can_write(&client_a, &path2, &content_hash));
        // Client B unaffected
        assert!(whitelist.can_write(&client_b, &path1, &content_hash));
    }

    #[test]
    fn test_different_paths_independent() {
        let mut whitelist = ReadWhitelist::new();
        let client = ClientId::stdio();
        let path1 = PathBuf::from("test1");
        let path2 = PathBuf::from("test2");
        let content_hash = hash("content");

        whitelist.mark_read(client.clone(), path1.clone(), content_hash.clone());

        assert!(whitelist.can_write(&client, &path1, &content_hash));
        // path2 was never read
        assert!(!whitelist.can_write(&client, &path2, &content_hash));
    }

    #[test]
    fn test_consecutive_edits_by_same_client() {
        let mut whitelist = ReadWhitelist::new();
        let client = ClientId::stdio();
        let path = PathBuf::from("test");

        let hash_v1 = hash("version 1");
        let hash_v2 = hash("version 2");
        let hash_v3 = hash("version 3");

        // Initial read
        whitelist.mark_read(client.clone(), path.clone(), hash_v1.clone());
        assert!(whitelist.can_write(&client, &path, &hash_v1));

        // First edit - update stored hash
        whitelist.mark_read(client.clone(), path.clone(), hash_v2.clone());
        assert!(whitelist.can_write(&client, &path, &hash_v2));

        // Second edit - update stored hash again
        whitelist.mark_read(client.clone(), path.clone(), hash_v3.clone());
        assert!(whitelist.can_write(&client, &path, &hash_v3));

        // Old hashes no longer valid
        assert!(!whitelist.can_write(&client, &path, &hash_v1));
        assert!(!whitelist.can_write(&client, &path, &hash_v2));
    }
}
