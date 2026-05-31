//! Filesystem-based KeyStorage implementation for the daemon.
//!
//! Reads and writes the ed25519 secret key as raw bytes to `.sync/daemon.key`
//! within the vault directory.

use async_trait::async_trait;
use std::path::{Path, PathBuf};
use sync_core::key_storage::{KeyStorage, KeyStorageError, Result};

const KEY_FILE: &str = ".sync/daemon.key";

/// Filesystem-based ed25519 key storage.
///
/// Reads/writes the key as 32 raw bytes to `.sync/daemon.key` inside the vault.
pub struct FileKeyStorage {
    path: PathBuf,
}

impl FileKeyStorage {
    /// Create a new `FileKeyStorage` rooted at `vault_path`.
    ///
    /// The key file will be read from and written to `{vault_path}/.sync/daemon.key`.
    pub fn new(vault_path: &Path) -> Self {
        Self {
            path: vault_path.join(KEY_FILE),
        }
    }
}

#[async_trait]
impl KeyStorage for FileKeyStorage {
    async fn load_key(&self) -> Result<Option<[u8; 32]>> {
        if !self.path.exists() {
            return Ok(None);
        }

        let bytes = tokio::fs::read(&self.path)
            .await
            .map_err(|e| KeyStorageError::Io(e.to_string()))?;

        let key: [u8; 32] = bytes.try_into().map_err(|_| KeyStorageError::InvalidKey)?;

        Ok(Some(key))
    }

    async fn save_key(&self, key: &[u8; 32]) -> Result<()> {
        if let Some(parent) = self.path.parent() {
            tokio::fs::create_dir_all(parent)
                .await
                .map_err(|e| KeyStorageError::Io(e.to_string()))?;
        }

        tokio::fs::write(&self.path, key.as_slice())
            .await
            .map_err(|e| KeyStorageError::Io(e.to_string()))?;

        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let perms = std::fs::Permissions::from_mode(0o600);
            tokio::fs::set_permissions(&self.path, perms)
                .await
                .map_err(|e| KeyStorageError::Io(e.to_string()))?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use sync_core::KeyStorage;
    use tempfile::TempDir;

    #[tokio::test]
    async fn test_load_key_returns_none_when_file_missing() {
        let temp_dir = TempDir::new().unwrap();
        let storage = FileKeyStorage::new(temp_dir.path());

        assert!(storage.load_key().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_save_and_load_key() {
        let temp_dir = TempDir::new().unwrap();
        let storage = FileKeyStorage::new(temp_dir.path());
        let key = [7u8; 32];

        storage.save_key(&key).await.unwrap();

        let loaded = storage.load_key().await.unwrap();
        assert_eq!(loaded, Some(key));
    }

    #[tokio::test]
    async fn test_save_creates_sync_directory() {
        let temp_dir = TempDir::new().unwrap();
        let storage = FileKeyStorage::new(temp_dir.path());
        let key = [1u8; 32];

        storage.save_key(&key).await.unwrap();

        assert!(temp_dir.path().join(".sync").exists());
        assert!(temp_dir.path().join(".sync/daemon.key").exists());
    }

    #[tokio::test]
    async fn test_key_file_contains_raw_bytes() {
        let temp_dir = TempDir::new().unwrap();
        let storage = FileKeyStorage::new(temp_dir.path());
        let key = [42u8; 32];

        storage.save_key(&key).await.unwrap();

        let raw = tokio::fs::read(temp_dir.path().join(".sync/daemon.key"))
            .await
            .unwrap();
        assert_eq!(raw, key.as_slice());
        assert_eq!(raw.len(), 32);
    }

    #[tokio::test]
    async fn test_load_key_rejects_wrong_length() {
        let temp_dir = TempDir::new().unwrap();
        let key_path = temp_dir.path().join(".sync/daemon.key");

        // Write garbage with wrong length
        tokio::fs::create_dir_all(key_path.parent().unwrap())
            .await
            .unwrap();
        tokio::fs::write(&key_path, b"too short").await.unwrap();

        let storage = FileKeyStorage::new(temp_dir.path());
        let result = storage.load_key().await;

        assert!(matches!(result, Err(KeyStorageError::InvalidKey)));
    }

    #[tokio::test]
    async fn test_load_or_generate_persists_across_instances() {
        let temp_dir = TempDir::new().unwrap();

        // First instance generates and saves a key
        let key1 = {
            let storage = FileKeyStorage::new(temp_dir.path());
            storage.load_or_generate().await.unwrap()
        };

        // Second instance should load the same key
        let key2 = {
            let storage = FileKeyStorage::new(temp_dir.path());
            storage.load_or_generate().await.unwrap()
        };

        assert_eq!(key1, key2);
    }
}
