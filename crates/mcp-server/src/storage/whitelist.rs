//! Read whitelist for tracking which clients can write to which files.
//!
//! Implements "must read before write" semantics:
//! - ReadNote marks a path as writable for that client
//! - File changes (via watcher) invalidate the path for all clients
//! - WriteNote/EditNote check the whitelist before allowing writes

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};

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

/// Tracks which clients have read which files, enabling "must read before write" checks.
///
/// Thread-safe via external synchronization (wrapped in RwLock by MemoryServer).
#[derive(Debug, Default)]
pub struct ReadWhitelist {
    /// Map of normalized path -> set of client IDs that have read it.
    entries: HashMap<PathBuf, HashSet<ClientId>>,
}

impl ReadWhitelist {
    /// Create a new empty whitelist.
    pub fn new() -> Self {
        Self::default()
    }

    /// Mark a path as read by a client, allowing subsequent writes.
    ///
    /// The path should be normalized (relative to vault root, no .md extension).
    pub fn mark_read(&mut self, client: ClientId, path: PathBuf) {
        self.entries
            .entry(path)
            .or_insert_with(HashSet::new)
            .insert(client);
    }

    /// Check if a client can write to a path (i.e., has read it since last change).
    pub fn can_write(&self, client: &ClientId, path: &Path) -> bool {
        self.entries
            .get(path)
            .map(|clients| clients.contains(client))
            .unwrap_or(false)
    }

    /// Invalidate a path for all clients (called when file changes externally).
    ///
    /// The path should be normalized (relative to vault root, no .md extension).
    pub fn invalidate_path(&mut self, path: &Path) {
        self.entries.remove(path);
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

    #[test]
    fn test_mark_read_allows_write() {
        let mut whitelist = ReadWhitelist::new();
        let client = ClientId::stdio();
        let path = PathBuf::from("test");

        assert!(!whitelist.can_write(&client, &path));

        whitelist.mark_read(client.clone(), path.clone());

        assert!(whitelist.can_write(&client, &path));
    }

    #[test]
    fn test_invalidate_path_blocks_write() {
        let mut whitelist = ReadWhitelist::new();
        let client = ClientId::stdio();
        let path = PathBuf::from("test");

        whitelist.mark_read(client.clone(), path.clone());
        assert!(whitelist.can_write(&client, &path));

        whitelist.invalidate_path(&path);
        assert!(!whitelist.can_write(&client, &path));
    }

    #[test]
    fn test_multiple_clients() {
        let mut whitelist = ReadWhitelist::new();
        let client_a = ClientId("client-a".to_string());
        let client_b = ClientId("client-b".to_string());
        let path = PathBuf::from("test");

        whitelist.mark_read(client_a.clone(), path.clone());

        assert!(whitelist.can_write(&client_a, &path));
        assert!(!whitelist.can_write(&client_b, &path));

        whitelist.mark_read(client_b.clone(), path.clone());

        assert!(whitelist.can_write(&client_a, &path));
        assert!(whitelist.can_write(&client_b, &path));
    }

    #[test]
    fn test_invalidate_path_affects_all_clients() {
        let mut whitelist = ReadWhitelist::new();
        let client_a = ClientId("client-a".to_string());
        let client_b = ClientId("client-b".to_string());
        let path = PathBuf::from("test");

        whitelist.mark_read(client_a.clone(), path.clone());
        whitelist.mark_read(client_b.clone(), path.clone());

        whitelist.invalidate_path(&path);

        assert!(!whitelist.can_write(&client_a, &path));
        assert!(!whitelist.can_write(&client_b, &path));
    }

    #[test]
    fn test_invalidate_client() {
        let mut whitelist = ReadWhitelist::new();
        let client_a = ClientId("client-a".to_string());
        let client_b = ClientId("client-b".to_string());
        let path1 = PathBuf::from("test1");
        let path2 = PathBuf::from("test2");

        whitelist.mark_read(client_a.clone(), path1.clone());
        whitelist.mark_read(client_a.clone(), path2.clone());
        whitelist.mark_read(client_b.clone(), path1.clone());

        whitelist.invalidate_client(&client_a);

        assert!(!whitelist.can_write(&client_a, &path1));
        assert!(!whitelist.can_write(&client_a, &path2));
        assert!(whitelist.can_write(&client_b, &path1));
    }

    #[test]
    fn test_different_paths_independent() {
        let mut whitelist = ReadWhitelist::new();
        let client = ClientId::stdio();
        let path1 = PathBuf::from("test1");
        let path2 = PathBuf::from("test2");

        whitelist.mark_read(client.clone(), path1.clone());

        assert!(whitelist.can_write(&client, &path1));
        assert!(!whitelist.can_write(&client, &path2));
    }
}
