//! KeyStorage trait for abstracting ed25519 secret key persistence.
//!
//! Implementations:
//! - `FileKeyStorage` (in sync-daemon) - Reads/writes `.sync/daemon.key` on the filesystem
//! - Plugin implementation (future) - Reads/writes via Obsidian's localStorage bridge

use async_trait::async_trait;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum KeyStorageError {
    #[error("Key file is corrupt or has unexpected length")]
    InvalidKey,

    #[error("IO error: {0}")]
    Io(String),
}

pub type Result<T> = std::result::Result<T, KeyStorageError>;

/// Abstracts loading and persisting an ed25519 secret key.
///
/// The key is stored as raw 32 bytes. Callers are responsible for
/// generating a new key when `load_key` returns `None`.
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg(not(target_arch = "wasm32"))]
pub trait KeyStorage: Send + Sync {
    /// Load an existing ed25519 secret key, or return `None` if no key exists yet.
    async fn load_key(&self) -> Result<Option<[u8; 32]>>;

    /// Persist an ed25519 secret key.
    async fn save_key(&self, key: &[u8; 32]) -> Result<()>;

    /// Load the existing key, or generate a new one and save it.
    ///
    /// The key is generated using cryptographically secure randomness.
    ///
    /// Note: This method is not atomic. If multiple processes call it
    /// simultaneously, they may each generate different keys. Callers
    /// should hold an appropriate lock (e.g., the daemon flock) before
    /// calling this method.
    async fn load_or_generate(&self) -> Result<[u8; 32]> {
        if let Some(key) = self.load_key().await? {
            return Ok(key);
        }

        let key = generate_key();
        self.save_key(&key).await?;
        Ok(key)
    }
}

/// Abstracts loading and persisting an ed25519 secret key (WASM version without Send + Sync).
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg(target_arch = "wasm32")]
pub trait KeyStorage {
    /// Load an existing ed25519 secret key, or return `None` if no key exists yet.
    async fn load_key(&self) -> Result<Option<[u8; 32]>>;

    /// Persist an ed25519 secret key.
    async fn save_key(&self, key: &[u8; 32]) -> Result<()>;

    /// Load the existing key, or generate a new one and save it.
    ///
    /// The key is generated using cryptographically secure randomness.
    ///
    /// Note: This method is not atomic. If multiple processes call it
    /// simultaneously, they may each generate different keys. Callers
    /// should hold an appropriate lock (e.g., the daemon flock) before
    /// calling this method.
    async fn load_or_generate(&self) -> Result<[u8; 32]> {
        if let Some(key) = self.load_key().await? {
            return Ok(key);
        }

        let key = generate_key();
        self.save_key(&key).await?;
        Ok(key)
    }
}

/// Generate a new random ed25519 secret key using cryptographically secure randomness.
fn generate_key() -> [u8; 32] {
    use rand::RngCore;
    let mut key = [0u8; 32];
    rand::rng().fill_bytes(&mut key);
    key
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::RwLock;

    /// In-memory KeyStorage for testing.
    struct InMemoryKeyStorage {
        key: RwLock<Option<[u8; 32]>>,
    }

    impl InMemoryKeyStorage {
        fn new() -> Self {
            Self {
                key: RwLock::new(None),
            }
        }

        fn new_with_key(key: [u8; 32]) -> Self {
            Self {
                key: RwLock::new(Some(key)),
            }
        }
    }

    #[async_trait]
    impl KeyStorage for InMemoryKeyStorage {
        async fn load_key(&self) -> Result<Option<[u8; 32]>> {
            Ok(*self.key.read().unwrap())
        }

        async fn save_key(&self, key: &[u8; 32]) -> Result<()> {
            *self.key.write().unwrap() = Some(*key);
            Ok(())
        }
    }

    #[tokio::test]
    async fn test_load_key_returns_none_when_empty() {
        let storage = InMemoryKeyStorage::new();
        assert!(storage.load_key().await.unwrap().is_none());
    }

    #[tokio::test]
    async fn test_save_and_load_key() {
        let storage = InMemoryKeyStorage::new();
        let key = [42u8; 32];

        storage.save_key(&key).await.unwrap();

        let loaded = storage.load_key().await.unwrap();
        assert_eq!(loaded, Some(key));
    }

    #[tokio::test]
    async fn test_load_or_generate_creates_key_when_none() {
        let storage = InMemoryKeyStorage::new();

        let key = storage.load_or_generate().await.unwrap();

        // Key should be saved so subsequent calls return the same key
        let loaded = storage.load_key().await.unwrap();
        assert_eq!(loaded, Some(key));
    }

    #[tokio::test]
    async fn test_load_or_generate_returns_existing_key() {
        let existing_key = [99u8; 32];
        let storage = InMemoryKeyStorage::new_with_key(existing_key);

        let key = storage.load_or_generate().await.unwrap();

        assert_eq!(key, existing_key);
    }

    #[tokio::test]
    async fn test_load_or_generate_is_stable() {
        let storage = InMemoryKeyStorage::new();

        let key1 = storage.load_or_generate().await.unwrap();
        let key2 = storage.load_or_generate().await.unwrap();

        // Both calls should return the same key
        assert_eq!(key1, key2);
    }

    #[tokio::test]
    async fn test_generated_key_is_32_bytes() {
        let storage = InMemoryKeyStorage::new();
        let key = storage.load_or_generate().await.unwrap();
        assert_eq!(key.len(), 32);
    }
}
