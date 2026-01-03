//! Storage trait definition and error types.

use std::path::PathBuf;

/// Errors that can occur during storage operations.
#[derive(Debug, Clone)]
pub enum StorageError {
    /// The requested note was not found
    NotFound { uri: String },
    /// The note already exists (for create operations)
    AlreadyExists { uri: String },
    /// Content hash mismatch during optimistic locking
    HashMismatch {
        uri: String,
        expected: String,
        actual: String,
    },
    /// Path validation failed (e.g., directory traversal attempt)
    InvalidPath { uri: String, reason: String },
    /// I/O error during storage operation
    IoError { message: String },
    /// Parent directory doesn't exist
    ParentNotFound { uri: String, parent: PathBuf },
}

impl std::fmt::Display for StorageError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            StorageError::NotFound { uri } => write!(f, "Note not found: {}", uri),
            StorageError::AlreadyExists { uri } => write!(f, "Note already exists: {}", uri),
            StorageError::HashMismatch {
                uri,
                expected,
                actual,
            } => write!(
                f,
                "Content changed since last read for {}: expected hash {}, found {}",
                uri, expected, actual
            ),
            StorageError::InvalidPath { uri, reason } => {
                write!(f, "Invalid path '{}': {}", uri, reason)
            }
            StorageError::IoError { message } => write!(f, "I/O error: {}", message),
            StorageError::ParentNotFound { uri, parent } => {
                write!(
                    f,
                    "Parent directory doesn't exist for '{}': {}",
                    uri,
                    parent.display()
                )
            }
        }
    }
}

impl std::error::Error for StorageError {}

impl From<std::io::Error> for StorageError {
    fn from(e: std::io::Error) -> Self {
        StorageError::IoError {
            message: e.to_string(),
        }
    }
}

/// Metadata returned when reading a note.
#[derive(Debug, Clone)]
pub struct NoteMetadata {
    /// Content hash for optimistic locking (SHA-256 of content)
    pub hash: String,
    /// The memory URI of the note (e.g., "knowledge/My Note")
    pub uri: String,
}

/// Result of a write operation.
#[derive(Debug, Clone)]
pub struct WriteResult {
    /// New content hash after write
    pub hash: String,
    /// The memory URI of the note
    pub uri: String,
}

/// Abstract storage backend for note access.
///
/// Implementations provide filesystem primitives for reading and writing notes.
/// The trait uses memory URIs (without .md extension) for note identification.
///
/// Current implementation: `LocalStorage` (filesystem)
/// Future implementation: `RemoteStorage` (HTTP to another MCP server)
#[async_trait::async_trait]
pub trait Storage: Send + Sync {
    /// Check if a note exists at the given memory URI.
    ///
    /// # Arguments
    /// * `uri` - Memory URI without extension (e.g., "knowledge/My Note")
    async fn exists(&self, uri: &str) -> Result<bool, StorageError>;

    /// Read note content.
    ///
    /// Returns the content and metadata including hash for optimistic locking.
    ///
    /// # Arguments
    /// * `uri` - Memory URI without extension
    async fn read(&self, uri: &str) -> Result<(String, NoteMetadata), StorageError>;

    /// Write note content.
    ///
    /// If `expected_hash` is provided, the write will fail if the current content
    /// hash doesn't match (optimistic locking).
    ///
    /// # Arguments
    /// * `uri` - Memory URI without extension
    /// * `content` - The new content to write
    /// * `expected_hash` - Optional hash for optimistic locking
    async fn write(
        &self,
        uri: &str,
        content: &str,
        expected_hash: Option<&str>,
    ) -> Result<WriteResult, StorageError>;

    /// Delete a note.
    ///
    /// # Arguments
    /// * `uri` - Memory URI without extension
    async fn delete(&self, uri: &str) -> Result<(), StorageError>;

    /// List notes under a prefix.
    ///
    /// Returns memory URIs (without extensions) for all notes matching the prefix.
    ///
    /// # Arguments
    /// * `prefix` - Path prefix (e.g., "knowledge/", "" for all)
    async fn list(&self, prefix: &str) -> Result<Vec<String>, StorageError>;

    /// Move/rename a note.
    ///
    /// # Arguments
    /// * `from` - Source memory URI
    /// * `to` - Destination memory URI
    async fn rename(&self, from: &str, to: &str) -> Result<(), StorageError>;
}
