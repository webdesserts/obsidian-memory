//! FileSystem trait abstraction for platform-independent file operations.
//!
//! Implementations:
//! - `InMemoryFs` - For testing
//! - `ObsidianFs` (in sync-wasm) - Uses Obsidian's Vault API via JS bridge
//! - `NativeFs` (in sync-daemon) - Uses tokio::fs
//!
//! Uses `target_arch = "wasm32"` for conditional compilation instead of feature flags
//! to avoid Cargo's feature unification issues when building the workspace.

use async_trait::async_trait;
use std::collections::HashMap;
use std::sync::RwLock;
use thiserror::Error;

#[derive(Debug, Error)]
pub enum FsError {
    #[error("File not found: {0}")]
    NotFound(String),

    #[error("Already exists: {0}")]
    AlreadyExists(String),

    #[error("Is a directory: {0}")]
    IsDirectory(String),

    #[error("Not a directory: {0}")]
    NotDirectory(String),

    #[error("IO error: {0}")]
    Io(String),
}

pub type Result<T> = std::result::Result<T, FsError>;

/// File metadata
#[derive(Debug, Clone)]
pub struct FileStat {
    /// Modification time in milliseconds since epoch
    pub mtime_millis: u64,
    /// File size in bytes
    pub size: u64,
    /// Whether this is a directory
    pub is_dir: bool,
}

/// Directory entry
#[derive(Debug, Clone)]
pub struct FileEntry {
    /// File or directory name (not full path)
    pub name: String,
    /// Whether this is a directory
    pub is_dir: bool,
}

/// Platform-independent filesystem abstraction.
///
/// On native platforms, implementations must be `Send + Sync` for use across threads.
/// On WASM (wasm32), these bounds are relaxed since WASM is single-threaded.
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg(not(target_arch = "wasm32"))]
pub trait FileSystem: Send + Sync {
    /// Read file contents
    async fn read(&self, path: &str) -> Result<Vec<u8>>;

    /// Write file contents (creates parent directories if needed)
    async fn write(&self, path: &str, content: &[u8]) -> Result<()>;

    /// List directory contents
    async fn list(&self, path: &str) -> Result<Vec<FileEntry>>;

    /// Delete file or empty directory
    async fn delete(&self, path: &str) -> Result<()>;

    /// Check if path exists
    async fn exists(&self, path: &str) -> Result<bool>;

    /// Get file metadata
    async fn stat(&self, path: &str) -> Result<FileStat>;

    /// Create directory (and parents if needed)
    async fn mkdir(&self, path: &str) -> Result<()>;
}

/// Platform-independent filesystem abstraction (WASM version without Send + Sync).
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg(target_arch = "wasm32")]
pub trait FileSystem {
    /// Read file contents
    async fn read(&self, path: &str) -> Result<Vec<u8>>;

    /// Write file contents (creates parent directories if needed)
    async fn write(&self, path: &str, content: &[u8]) -> Result<()>;

    /// List directory contents
    async fn list(&self, path: &str) -> Result<Vec<FileEntry>>;

    /// Delete file or empty directory
    async fn delete(&self, path: &str) -> Result<()>;

    /// Check if path exists
    async fn exists(&self, path: &str) -> Result<bool>;

    /// Get file metadata
    async fn stat(&self, path: &str) -> Result<FileStat>;

    /// Create directory (and parents if needed)
    async fn mkdir(&self, path: &str) -> Result<()>;
}

/// In-memory filesystem for testing
pub struct InMemoryFs {
    files: RwLock<HashMap<String, Vec<u8>>>,
    dirs: RwLock<HashMap<String, ()>>,
    /// Tracks file modification times (path -> mtime in ms)
    mtimes: RwLock<HashMap<String, u64>>,
}

impl InMemoryFs {
    pub fn new() -> Self {
        let mut dirs = HashMap::new();
        dirs.insert(String::new(), ()); // Root directory
        Self {
            files: RwLock::new(HashMap::new()),
            dirs: RwLock::new(dirs),
            mtimes: RwLock::new(HashMap::new()),
        }
    }

    /// Set a specific mtime for testing "latest wins" scenarios
    pub fn set_mtime(&self, path: &str, mtime: u64) {
        let path = Self::normalize_path(path);
        let mut mtimes = self.mtimes.write().unwrap();
        mtimes.insert(path, mtime);
    }

    /// Get current time in milliseconds (monotonically increasing for tests)
    fn current_time_ms() -> u64 {
        use std::time::{SystemTime, UNIX_EPOCH};
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_millis() as u64
    }

    fn normalize_path(path: &str) -> String {
        path.trim_matches('/').to_string()
    }

    fn parent_path(path: &str) -> Option<String> {
        let normalized = Self::normalize_path(path);
        if normalized.is_empty() {
            None
        } else {
            match normalized.rfind('/') {
                Some(pos) => Some(normalized[..pos].to_string()),
                None => Some(String::new()),
            }
        }
    }
}

impl Default for InMemoryFs {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
impl FileSystem for InMemoryFs {
    async fn read(&self, path: &str) -> Result<Vec<u8>> {
        let path = Self::normalize_path(path);
        let files = self.files.read().unwrap();
        files
            .get(&path)
            .cloned()
            .ok_or_else(|| FsError::NotFound(path))
    }

    async fn write(&self, path: &str, content: &[u8]) -> Result<()> {
        let path = Self::normalize_path(path);

        // Create parent directories
        if let Some(parent) = Self::parent_path(&path) {
            self.mkdir(&parent).await?;
        }

        let mut files = self.files.write().unwrap();
        files.insert(path.clone(), content.to_vec());
        drop(files);

        // Update mtime
        let mut mtimes = self.mtimes.write().unwrap();
        mtimes.insert(path, Self::current_time_ms());
        Ok(())
    }

    async fn list(&self, path: &str) -> Result<Vec<FileEntry>> {
        let path = Self::normalize_path(path);
        let prefix = if path.is_empty() {
            String::new()
        } else {
            format!("{}/", path)
        };

        let dirs = self.dirs.read().unwrap();
        if !path.is_empty() && !dirs.contains_key(&path) {
            return Err(FsError::NotFound(path));
        }

        let mut entries = Vec::new();
        let mut seen = std::collections::HashSet::new();

        // List files
        let files = self.files.read().unwrap();
        for file_path in files.keys() {
            if let Some(rest) = file_path.strip_prefix(&prefix) {
                let name = rest.split('/').next().unwrap();
                if !rest.contains('/') && seen.insert(name.to_string()) {
                    entries.push(FileEntry {
                        name: name.to_string(),
                        is_dir: false,
                    });
                }
            } else if prefix.is_empty() && !file_path.contains('/') {
                if seen.insert(file_path.clone()) {
                    entries.push(FileEntry {
                        name: file_path.clone(),
                        is_dir: false,
                    });
                }
            }
        }

        // List subdirectories
        for dir_path in dirs.keys() {
            if let Some(rest) = dir_path.strip_prefix(&prefix) {
                let name = rest.split('/').next().unwrap();
                if !name.is_empty() && seen.insert(name.to_string()) {
                    entries.push(FileEntry {
                        name: name.to_string(),
                        is_dir: true,
                    });
                }
            } else if prefix.is_empty() && !dir_path.is_empty() && !dir_path.contains('/') {
                if seen.insert(dir_path.clone()) {
                    entries.push(FileEntry {
                        name: dir_path.clone(),
                        is_dir: true,
                    });
                }
            }
        }

        Ok(entries)
    }

    async fn delete(&self, path: &str) -> Result<()> {
        let path = Self::normalize_path(path);

        // Try to delete as file first
        {
            let mut files = self.files.write().unwrap();
            if files.remove(&path).is_some() {
                return Ok(());
            }
        }

        // Try to delete as directory
        {
            let mut dirs = self.dirs.write().unwrap();
            if dirs.remove(&path).is_some() {
                return Ok(());
            }
        }

        Err(FsError::NotFound(path))
    }

    async fn exists(&self, path: &str) -> Result<bool> {
        let path = Self::normalize_path(path);
        let files = self.files.read().unwrap();
        let dirs = self.dirs.read().unwrap();
        Ok(files.contains_key(&path) || dirs.contains_key(&path))
    }

    async fn stat(&self, path: &str) -> Result<FileStat> {
        let path = Self::normalize_path(path);

        let files = self.files.read().unwrap();
        if let Some(content) = files.get(&path) {
            let mtimes = self.mtimes.read().unwrap();
            let mtime = mtimes.get(&path).copied().unwrap_or(0);
            return Ok(FileStat {
                mtime_millis: mtime,
                size: content.len() as u64,
                is_dir: false,
            });
        }

        let dirs = self.dirs.read().unwrap();
        if dirs.contains_key(&path) {
            return Ok(FileStat {
                mtime_millis: 0,
                size: 0,
                is_dir: true,
            });
        }

        Err(FsError::NotFound(path))
    }

    async fn mkdir(&self, path: &str) -> Result<()> {
        let path = Self::normalize_path(path);
        if path.is_empty() {
            return Ok(()); // Root always exists
        }

        // Create parent first
        if let Some(parent) = Self::parent_path(&path) {
            Box::pin(self.mkdir(&parent)).await?;
        }

        let mut dirs = self.dirs.write().unwrap();
        dirs.insert(path, ());
        Ok(())
    }
}

// Implement FileSystem for Arc<T> where T: FileSystem
// This allows sharing a filesystem between multiple Vaults in tests
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg(not(target_arch = "wasm32"))]
impl<T: FileSystem + Send + Sync> FileSystem for std::sync::Arc<T> {
    async fn read(&self, path: &str) -> Result<Vec<u8>> {
        (**self).read(path).await
    }

    async fn write(&self, path: &str, content: &[u8]) -> Result<()> {
        (**self).write(path, content).await
    }

    async fn list(&self, path: &str) -> Result<Vec<FileEntry>> {
        (**self).list(path).await
    }

    async fn delete(&self, path: &str) -> Result<()> {
        (**self).delete(path).await
    }

    async fn exists(&self, path: &str) -> Result<bool> {
        (**self).exists(path).await
    }

    async fn stat(&self, path: &str) -> Result<FileStat> {
        (**self).stat(path).await
    }

    async fn mkdir(&self, path: &str) -> Result<()> {
        (**self).mkdir(path).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_inmemory_fs_basic_operations() {
        let fs = InMemoryFs::new();

        // Write a file
        fs.write("test.txt", b"hello world").await.unwrap();

        // Read it back
        let content = fs.read("test.txt").await.unwrap();
        assert_eq!(content, b"hello world");

        // Check exists
        assert!(fs.exists("test.txt").await.unwrap());
        assert!(!fs.exists("nonexistent.txt").await.unwrap());

        // Delete
        fs.delete("test.txt").await.unwrap();
        assert!(!fs.exists("test.txt").await.unwrap());
    }

    #[tokio::test]
    async fn test_inmemory_fs_directories() {
        let fs = InMemoryFs::new();

        // Write creates parent directories
        fs.write("a/b/c.txt", b"content").await.unwrap();

        // Parent directories exist
        assert!(fs.exists("a").await.unwrap());
        assert!(fs.exists("a/b").await.unwrap());

        // List directory
        let entries = fs.list("a").await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "b");
        assert!(entries[0].is_dir);

        let entries = fs.list("a/b").await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].name, "c.txt");
        assert!(!entries[0].is_dir);
    }
}
