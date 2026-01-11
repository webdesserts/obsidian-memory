//! Native filesystem implementation using tokio::fs.

use async_trait::async_trait;
use std::path::PathBuf;
use sync_core::fs::{FileEntry, FileStat, FileSystem, FsError, Result};
use tokio::fs;

/// Native filesystem implementation for the daemon
pub struct NativeFs {
    base_path: PathBuf,
}

impl NativeFs {
    pub fn new(base_path: PathBuf) -> Self {
        Self { base_path }
    }

    fn full_path(&self, path: &str) -> PathBuf {
        if path.is_empty() {
            self.base_path.clone()
        } else {
            self.base_path.join(path)
        }
    }
}

#[async_trait]
impl FileSystem for NativeFs {
    async fn read(&self, path: &str) -> Result<Vec<u8>> {
        let full_path = self.full_path(path);
        fs::read(&full_path)
            .await
            .map_err(|e| FsError::Io(e.to_string()))
    }

    async fn write(&self, path: &str, content: &[u8]) -> Result<()> {
        let full_path = self.full_path(path);

        // Create parent directories if needed
        if let Some(parent) = full_path.parent() {
            fs::create_dir_all(parent)
                .await
                .map_err(|e| FsError::Io(e.to_string()))?;
        }

        fs::write(&full_path, content)
            .await
            .map_err(|e| FsError::Io(e.to_string()))
    }

    async fn list(&self, path: &str) -> Result<Vec<FileEntry>> {
        let full_path = self.full_path(path);
        let mut entries = Vec::new();

        let mut dir = fs::read_dir(&full_path)
            .await
            .map_err(|e| FsError::Io(e.to_string()))?;

        while let Some(entry) = dir
            .next_entry()
            .await
            .map_err(|e| FsError::Io(e.to_string()))?
        {
            let name = entry.file_name().to_string_lossy().to_string();
            let metadata = entry
                .metadata()
                .await
                .map_err(|e| FsError::Io(e.to_string()))?;

            entries.push(FileEntry {
                name,
                is_dir: metadata.is_dir(),
            });
        }

        Ok(entries)
    }

    async fn delete(&self, path: &str) -> Result<()> {
        let full_path = self.full_path(path);
        let metadata = fs::metadata(&full_path)
            .await
            .map_err(|e| FsError::Io(e.to_string()))?;

        if metadata.is_dir() {
            fs::remove_dir(&full_path)
                .await
                .map_err(|e| FsError::Io(e.to_string()))
        } else {
            fs::remove_file(&full_path)
                .await
                .map_err(|e| FsError::Io(e.to_string()))
        }
    }

    async fn exists(&self, path: &str) -> Result<bool> {
        let full_path = self.full_path(path);
        Ok(full_path.exists())
    }

    async fn stat(&self, path: &str) -> Result<FileStat> {
        let full_path = self.full_path(path);
        let metadata = fs::metadata(&full_path)
            .await
            .map_err(|e| FsError::Io(e.to_string()))?;

        let mtime_millis = metadata
            .modified()
            .map(|t| {
                t.duration_since(std::time::UNIX_EPOCH)
                    .map(|d| d.as_millis() as u64)
                    .unwrap_or(0)
            })
            .unwrap_or(0);

        Ok(FileStat {
            mtime_millis,
            size: metadata.len(),
            is_dir: metadata.is_dir(),
        })
    }

    async fn mkdir(&self, path: &str) -> Result<()> {
        let full_path = self.full_path(path);
        fs::create_dir_all(&full_path)
            .await
            .map_err(|e| FsError::Io(e.to_string()))
    }
}
