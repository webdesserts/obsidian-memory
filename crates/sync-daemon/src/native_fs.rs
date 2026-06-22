//! Native filesystem implementation using tokio::fs.

use async_trait::async_trait;
use std::io;
use std::path::PathBuf;
use tokio::fs;
use vault_sync::fs::{FileEntry, FileStat, FileSystem, FsError, Result};

/// Map a tokio/std io error to FsError, preserving NotFound so callers
/// (e.g. the reconcile skip-deleted guard) can match on it. The `InMemoryFs`
/// test double already returns NotFound; this closes that prod/test gap.
fn map_io_err(path: &str, e: io::Error) -> FsError {
    match e.kind() {
        io::ErrorKind::NotFound => FsError::NotFound(path.to_string()),
        // AlreadyExists arm: pre-emptive parity with ObsidianFs/InMemoryFs — no
        // current caller in NativeFs branches on this variant, but having it here
        // keeps all three implementations consistent.
        io::ErrorKind::AlreadyExists => FsError::AlreadyExists(path.to_string()),
        _ => FsError::Io(e.to_string()),
    }
}

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
            .map_err(|e| map_io_err(path, e))
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
            .map_err(|e| map_io_err(path, e))?;

        if metadata.is_dir() {
            fs::remove_dir(&full_path)
                .await
                .map_err(|e| map_io_err(path, e))
        } else {
            fs::remove_file(&full_path)
                .await
                .map_err(|e| map_io_err(path, e))
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
            .map_err(|e| map_io_err(path, e))?;

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

    async fn rename(&self, from: &str, to: &str) -> Result<()> {
        let from_path = self.full_path(from);
        let to_path = self.full_path(to);
        // The error is keyed to `from`. A destination-side ENOENT (e.g. missing parent
        // directory) also surfaces as NotFound(from) — acceptable because no caller
        // inspects the path in the error to distinguish the two cases.
        fs::rename(&from_path, &to_path)
            .await
            .map_err(|e| map_io_err(from, e))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;
    use vault_sync::fs::conformance;

    /// Verify that `NativeFs` upholds the full `FileSystem` error contract.
    ///
    /// This replaces the standalone read/stat NotFound unit tests, which are now
    /// covered (along with delete, rename, and round-trip cases) by the shared suite.
    #[tokio::test]
    async fn native_fs_passes_conformance_suite() {
        let dir = tempdir().expect("tempdir");
        let fs = NativeFs::new(dir.path().to_path_buf());
        conformance::assert_fs_contract(&fs).await;
    }
}
