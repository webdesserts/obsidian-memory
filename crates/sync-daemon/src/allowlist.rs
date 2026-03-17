//! Filesystem-backed allowlist storage for the sync daemon.
//!
//! Stores the list of authorized peers in `.sync/allowlist.json` within the vault.

use async_trait::async_trait;
use std::fs;
use std::path::{Path, PathBuf};
use sync_core::allowlist::{AllowedPeer, AllowlistError, AllowlistStorage, Result as AllowlistResult};

/// Filesystem implementation of AllowlistStorage.
///
/// Reads and writes `.sync/allowlist.json` within the vault directory.
/// Each call reads/writes the full file — the list is short enough that
/// in-memory caching is not needed.
pub struct FileAllowlistStorage {
    path: PathBuf,
}

impl FileAllowlistStorage {
    /// Create storage pointing at `.sync/allowlist.json` within `vault_path`.
    pub fn new(vault_path: &Path) -> Self {
        Self {
            path: vault_path.join(".sync/allowlist.json"),
        }
    }
}

#[async_trait]
impl AllowlistStorage for FileAllowlistStorage {
    async fn list_peers(&self) -> AllowlistResult<Vec<AllowedPeer>> {
        if !self.path.exists() {
            return Ok(Vec::new());
        }

        let contents = fs::read_to_string(&self.path)
            .map_err(|e| AllowlistError::Io(e.to_string()))?;

        serde_json::from_str(&contents).map_err(|_| AllowlistError::InvalidData)
    }

    async fn save_peers(&self, peers: &[AllowedPeer]) -> AllowlistResult<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent).map_err(|e| AllowlistError::Io(e.to_string()))?;
        }

        let contents =
            serde_json::to_string_pretty(peers).map_err(|_| AllowlistError::InvalidData)?;

        fs::write(&self.path, contents).map_err(|e| AllowlistError::Io(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sync_core::PeerId;
    use tempfile::TempDir;

    fn make_storage(dir: &TempDir) -> FileAllowlistStorage {
        FileAllowlistStorage::new(dir.path())
    }

    #[tokio::test]
    async fn test_list_empty_when_no_file() {
        let dir = TempDir::new().unwrap();
        let storage = make_storage(&dir);

        let peers = storage.list_peers().await.unwrap();
        assert!(peers.is_empty());
    }

    #[tokio::test]
    async fn test_add_and_persist_peer() {
        let dir = TempDir::new().unwrap();
        let storage = make_storage(&dir);
        let id = PeerId::generate();

        storage.add_peer(id, "umbra").await.unwrap();

        // File should exist
        let path = dir.path().join(".sync/allowlist.json");
        assert!(path.exists());

        // Reload in a new instance — should find the peer
        let storage2 = make_storage(&dir);
        let peers = storage2.list_peers().await.unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].node_id, id);
        assert_eq!(peers[0].device_name, "umbra");
    }

    #[tokio::test]
    async fn test_remove_peer_persists() {
        let dir = TempDir::new().unwrap();
        let storage = make_storage(&dir);
        let a = PeerId::generate();
        let b = PeerId::generate();

        storage.add_peer(a, "device-a").await.unwrap();
        storage.add_peer(b, "device-b").await.unwrap();
        storage.remove_peer(&a).await.unwrap();

        let storage2 = make_storage(&dir);
        let peers = storage2.list_peers().await.unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].node_id, b);
    }

    #[tokio::test]
    async fn test_is_allowed_checks_file() {
        let dir = TempDir::new().unwrap();
        let storage = make_storage(&dir);
        let id = PeerId::generate();
        let other = PeerId::generate();

        storage.add_peer(id, "my-device").await.unwrap();

        let storage2 = make_storage(&dir);
        assert!(storage2.is_allowed(&id).await.unwrap());
        assert!(!storage2.is_allowed(&other).await.unwrap());
    }

    #[tokio::test]
    async fn test_update_last_seen_persists() {
        let dir = TempDir::new().unwrap();
        let storage = make_storage(&dir);
        let id = PeerId::generate();

        storage.add_peer(id, "umbra").await.unwrap();
        storage.update_last_seen(&id, 9_999_999).await.unwrap();

        let storage2 = make_storage(&dir);
        let peers = storage2.list_peers().await.unwrap();
        assert_eq!(peers[0].last_seen, Some(9_999_999));
    }

    #[tokio::test]
    async fn test_creates_sync_dir_if_missing() {
        let dir = TempDir::new().unwrap();
        // Don't pre-create .sync dir
        let storage = make_storage(&dir);
        let id = PeerId::generate();

        storage.add_peer(id, "device").await.unwrap();

        assert!(dir.path().join(".sync").is_dir());
    }
}
