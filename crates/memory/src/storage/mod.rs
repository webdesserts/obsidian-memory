//! Storage abstraction for note access.
//!
//! Provides a trait for filesystem operations that can be backed by local filesystem
//! or remote HTTP in the future. This enables the same MCP tools to work both locally
//! (for Claude Code) and remotely (for Claude iOS via home server).
//!
//! The Storage layer operates on memory URIs (e.g., "knowledge/My Note") and returns
//! raw content. Higher-level concerns like wiki-link resolution stay in the MCP tools.

mod content_hash;
mod file;
mod traits;

pub use content_hash::ContentHash;
pub use file::FileStorage;
// kept: NoteMetadata/WriteResult have no production consumer outside this
// crate's own Storage impls, but need to be `pub` here so test-only Storage
// wrappers (e.g. edit_note.rs's ForceWriteHashMismatchStorage) can implement
// the trait's exact read()/write() signatures.
#[allow(unused_imports)]
pub use traits::{NoteMetadata, Storage, StorageError, WriteResult};
