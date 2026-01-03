//! Filesystem storage implementation.

use std::path::{Path, PathBuf};

use obsidian_fs::validate_relative_path;
use rand::Rng;
use sha2::{Digest, Sha256};
use tokio::fs;

use super::traits::{NoteMetadata, Storage, StorageError, WriteResult};

/// Filesystem storage backend.
///
/// Stores notes as markdown files in the vault directory.
/// Uses SHA-256 hashing for optimistic locking.
pub struct FileStorage {
    vault_path: PathBuf,
}

impl FileStorage {
    /// Create a new FileStorage for the given vault path.
    pub fn new(vault_path: PathBuf) -> Self {
        Self { vault_path }
    }

    /// Convert a memory URI to a filesystem path.
    ///
    /// Validates the path to prevent directory traversal attacks.
    fn uri_to_path(&self, uri: &str) -> Result<PathBuf, StorageError> {
        // Validate the URI as a relative path
        let clean = validate_relative_path(uri).map_err(|e| StorageError::InvalidPath {
            uri: uri.to_string(),
            reason: e.to_string(),
        })?;

        // Add .md extension if not present
        let with_ext = if clean.ends_with(".md") {
            clean
        } else {
            format!("{}.md", clean)
        };

        Ok(self.vault_path.join(with_ext))
    }

    /// Convert a filesystem path back to a memory URI.
    fn path_to_uri(&self, path: &Path) -> Option<String> {
        path.strip_prefix(&self.vault_path)
            .ok()
            .and_then(|rel| rel.to_str())
            .map(|s| s.strip_suffix(".md").unwrap_or(s).to_string())
    }

    /// Compute SHA-256 hash of content.
    fn compute_hash(content: &str) -> String {
        let mut hasher = Sha256::new();
        hasher.update(content.as_bytes());
        format!("{:x}", hasher.finalize())
    }

    /// Generate a random hex string for temp file names.
    fn random_hex() -> String {
        let bytes: [u8; 16] = rand::rng().random();
        hex::encode(bytes)
    }

    /// Atomic write using temp file + rename.
    ///
    /// This prevents race conditions and ensures the file is either
    /// fully written or not modified at all.
    async fn atomic_write(path: &Path, content: &str) -> Result<(), std::io::Error> {
        let temp_path = path.with_extension(format!("{}.tmp", Self::random_hex()));

        // Write to temp file
        if let Err(e) = fs::write(&temp_path, content).await {
            // Clean up temp file if write failed
            let _ = fs::remove_file(&temp_path).await;
            return Err(e);
        }

        // Atomic rename to target
        if let Err(e) = fs::rename(&temp_path, path).await {
            // Clean up temp file if rename failed
            let _ = fs::remove_file(&temp_path).await;
            return Err(e);
        }

        Ok(())
    }
}

#[async_trait::async_trait]
impl Storage for FileStorage {
    async fn exists(&self, uri: &str) -> Result<bool, StorageError> {
        let path = self.uri_to_path(uri)?;
        Ok(path.exists())
    }

    async fn read(&self, uri: &str) -> Result<(String, NoteMetadata), StorageError> {
        let path = self.uri_to_path(uri)?;

        let content = fs::read_to_string(&path).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                StorageError::NotFound {
                    uri: uri.to_string(),
                }
            } else {
                StorageError::from(e)
            }
        })?;

        let hash = Self::compute_hash(&content);
        let metadata = NoteMetadata {
            hash,
            uri: uri.to_string(),
        };

        Ok((content, metadata))
    }

    async fn write(
        &self,
        uri: &str,
        content: &str,
        expected_hash: Option<&str>,
    ) -> Result<WriteResult, StorageError> {
        let path = self.uri_to_path(uri)?;

        // Optimistic locking: check hash if provided
        if let Some(expected) = expected_hash {
            match fs::read_to_string(&path).await {
                Ok(current_content) => {
                    let actual_hash = Self::compute_hash(&current_content);
                    if actual_hash != expected {
                        return Err(StorageError::HashMismatch {
                            uri: uri.to_string(),
                            expected: expected.to_string(),
                            actual: actual_hash,
                        });
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    // File doesn't exist - hash check fails if we expected a specific hash
                    return Err(StorageError::HashMismatch {
                        uri: uri.to_string(),
                        expected: expected.to_string(),
                        actual: "(file does not exist)".to_string(),
                    });
                }
                Err(e) => return Err(StorageError::from(e)),
            }
        }

        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            if !parent.exists() {
                return Err(StorageError::ParentNotFound {
                    uri: uri.to_string(),
                    parent: parent.to_path_buf(),
                });
            }
        }

        // Atomic write using temp file + rename
        Self::atomic_write(&path, content).await?;

        let new_hash = Self::compute_hash(content);
        Ok(WriteResult {
            hash: new_hash,
            uri: uri.to_string(),
        })
    }

    async fn delete(&self, uri: &str) -> Result<(), StorageError> {
        let path = self.uri_to_path(uri)?;

        fs::remove_file(&path).await.map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                StorageError::NotFound {
                    uri: uri.to_string(),
                }
            } else {
                StorageError::from(e)
            }
        })
    }

    async fn list(&self, prefix: &str) -> Result<Vec<String>, StorageError> {
        let search_dir = if prefix.is_empty() {
            self.vault_path.clone()
        } else {
            // Validate prefix path
            let clean = validate_relative_path(prefix).map_err(|e| StorageError::InvalidPath {
                uri: prefix.to_string(),
                reason: e.to_string(),
            })?;
            self.vault_path.join(clean)
        };

        if !search_dir.exists() {
            return Ok(Vec::new());
        }

        let mut notes = Vec::new();
        self.list_recursive(&search_dir, &mut notes).await?;
        Ok(notes)
    }

    async fn rename(&self, from: &str, to: &str) -> Result<(), StorageError> {
        let from_path = self.uri_to_path(from)?;
        let to_path = self.uri_to_path(to)?;

        // Check source exists
        if !from_path.exists() {
            return Err(StorageError::NotFound {
                uri: from.to_string(),
            });
        }

        // Check destination doesn't exist
        if to_path.exists() {
            return Err(StorageError::AlreadyExists {
                uri: to.to_string(),
            });
        }

        // Ensure destination parent exists
        if let Some(parent) = to_path.parent() {
            if !parent.exists() {
                return Err(StorageError::ParentNotFound {
                    uri: to.to_string(),
                    parent: parent.to_path_buf(),
                });
            }
        }

        fs::rename(&from_path, &to_path).await?;
        Ok(())
    }
}

impl FileStorage {
    /// Recursively list markdown files in a directory.
    async fn list_recursive(
        &self,
        dir: &Path,
        notes: &mut Vec<String>,
    ) -> Result<(), StorageError> {
        let mut entries = fs::read_dir(dir).await?;

        while let Some(entry) = entries.next_entry().await? {
            let path = entry.path();
            let file_name = entry.file_name();
            let file_name_str = file_name.to_string_lossy();

            // Skip hidden files and directories
            if file_name_str.starts_with('.') {
                continue;
            }

            let file_type = entry.file_type().await?;
            if file_type.is_dir() {
                Box::pin(self.list_recursive(&path, notes)).await?;
            } else if file_type.is_file() && file_name_str.ends_with(".md") {
                if let Some(uri) = self.path_to_uri(&path) {
                    notes.push(uri);
                }
            }
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    async fn create_test_storage() -> (TempDir, FileStorage) {
        let temp_dir = TempDir::new().unwrap();
        let storage = FileStorage::new(temp_dir.path().to_path_buf());
        (temp_dir, storage)
    }

    #[tokio::test]
    async fn test_exists_returns_false_for_nonexistent() {
        let (_temp, storage) = create_test_storage().await;
        assert!(!storage.exists("nonexistent").await.unwrap());
    }

    #[tokio::test]
    async fn test_write_and_read() {
        let (temp, storage) = create_test_storage().await;

        // Create a note
        let result = storage.write("test", "Hello, world!", None).await.unwrap();
        assert!(!result.hash.is_empty());

        // Verify file exists
        assert!(temp.path().join("test.md").exists());

        // Read it back
        let (content, metadata) = storage.read("test").await.unwrap();
        assert_eq!(content, "Hello, world!");
        assert_eq!(metadata.uri, "test");
        assert_eq!(metadata.hash, result.hash);
    }

    #[tokio::test]
    async fn test_read_nonexistent_returns_not_found() {
        let (_temp, storage) = create_test_storage().await;

        let result = storage.read("nonexistent").await;
        assert!(matches!(result, Err(StorageError::NotFound { .. })));
    }

    #[tokio::test]
    async fn test_optimistic_locking() {
        let (_temp, storage) = create_test_storage().await;

        // Write initial content
        let result = storage.write("test", "version 1", None).await.unwrap();
        let hash_v1 = result.hash;

        // Update with correct hash succeeds
        let result = storage
            .write("test", "version 2", Some(&hash_v1))
            .await
            .unwrap();
        let hash_v2 = result.hash;
        assert_ne!(hash_v1, hash_v2);

        // Update with stale hash fails
        let result = storage.write("test", "version 3", Some(&hash_v1)).await;
        assert!(matches!(result, Err(StorageError::HashMismatch { .. })));

        // Verify content wasn't changed
        let (content, _) = storage.read("test").await.unwrap();
        assert_eq!(content, "version 2");
    }

    #[tokio::test]
    async fn test_write_to_subdirectory() {
        let (temp, storage) = create_test_storage().await;

        // Create subdirectory first
        fs::create_dir(temp.path().join("knowledge")).await.unwrap();

        // Write to subdirectory
        storage
            .write("knowledge/test", "content", None)
            .await
            .unwrap();

        // Verify
        assert!(temp.path().join("knowledge/test.md").exists());
        let (content, _) = storage.read("knowledge/test").await.unwrap();
        assert_eq!(content, "content");
    }

    #[tokio::test]
    async fn test_write_fails_if_parent_missing() {
        let (_temp, storage) = create_test_storage().await;

        let result = storage
            .write("missing/parent/test", "content", None)
            .await;
        assert!(matches!(result, Err(StorageError::ParentNotFound { .. })));
    }

    #[tokio::test]
    async fn test_delete() {
        let (_temp, storage) = create_test_storage().await;

        // Create and verify
        storage.write("to-delete", "content", None).await.unwrap();
        assert!(storage.exists("to-delete").await.unwrap());

        // Delete
        storage.delete("to-delete").await.unwrap();
        assert!(!storage.exists("to-delete").await.unwrap());
    }

    #[tokio::test]
    async fn test_delete_nonexistent_returns_not_found() {
        let (_temp, storage) = create_test_storage().await;

        let result = storage.delete("nonexistent").await;
        assert!(matches!(result, Err(StorageError::NotFound { .. })));
    }

    #[tokio::test]
    async fn test_list() {
        let (temp, storage) = create_test_storage().await;

        // Create some notes
        storage.write("note1", "content", None).await.unwrap();
        storage.write("note2", "content", None).await.unwrap();

        // Create subdirectory with notes
        fs::create_dir(temp.path().join("sub")).await.unwrap();
        storage.write("sub/note3", "content", None).await.unwrap();

        // List all
        let mut all = storage.list("").await.unwrap();
        all.sort();
        assert_eq!(all, vec!["note1", "note2", "sub/note3"]);

        // List subdirectory only
        let sub = storage.list("sub").await.unwrap();
        assert_eq!(sub, vec!["sub/note3"]);
    }

    #[tokio::test]
    async fn test_rename() {
        let (_temp, storage) = create_test_storage().await;

        // Create note
        storage.write("old-name", "content", None).await.unwrap();

        // Rename
        storage.rename("old-name", "new-name").await.unwrap();

        // Verify
        assert!(!storage.exists("old-name").await.unwrap());
        assert!(storage.exists("new-name").await.unwrap());
        let (content, _) = storage.read("new-name").await.unwrap();
        assert_eq!(content, "content");
    }

    #[tokio::test]
    async fn test_rename_to_existing_fails() {
        let (_temp, storage) = create_test_storage().await;

        storage.write("note1", "content 1", None).await.unwrap();
        storage.write("note2", "content 2", None).await.unwrap();

        let result = storage.rename("note1", "note2").await;
        assert!(matches!(result, Err(StorageError::AlreadyExists { .. })));
    }

    #[tokio::test]
    async fn test_rejects_directory_traversal() {
        let (_temp, storage) = create_test_storage().await;

        let result = storage.read("../etc/passwd").await;
        assert!(matches!(result, Err(StorageError::InvalidPath { .. })));

        let result = storage.write("../../evil", "content", None).await;
        assert!(matches!(result, Err(StorageError::InvalidPath { .. })));
    }

    #[tokio::test]
    async fn test_handles_md_extension_in_uri() {
        let (_temp, storage) = create_test_storage().await;

        // Write without extension
        storage.write("test", "content", None).await.unwrap();

        // Read with extension also works (URI normalization)
        let (content, _) = storage.read("test.md").await.unwrap();
        assert_eq!(content, "content");
    }
}
