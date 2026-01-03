//! Storage abstraction for note access.
//!
//! Provides a trait for filesystem operations that can be backed by local filesystem
//! or remote HTTP in the future. This enables the same MCP tools to work both locally
//! (for Claude Code) and remotely (for Claude iOS via home server).
//!
//! The Storage layer operates on memory URIs (e.g., "knowledge/My Note") and returns
//! raw content. Higher-level concerns like wiki-link resolution stay in the MCP tools.

mod file;
mod traits;
mod whitelist;

pub use file::FileStorage;
pub use traits::{Storage, StorageError};
pub use whitelist::{ClientId, ReadWhitelist};
