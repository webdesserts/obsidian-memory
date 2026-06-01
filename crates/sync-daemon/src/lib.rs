//! sync-daemon library: Exposes internal modules for testing.
//!
//! This is a thin library layer over the daemon components,
//! allowing integration tests to access internal types.

pub mod allowlist;
pub mod daemon;
pub mod daemon_lock;
pub mod http;
pub mod key_storage;
pub mod native_fs;
pub mod pair;
pub mod pair_api;
pub mod pair_shared;
pub mod persistence;
pub mod relay;
pub mod watcher;

// Re-export key types for convenience
pub use allowlist::FileAllowlistStorage;
pub use key_storage::FileKeyStorage;
pub use native_fs::NativeFs;
pub use watcher::{FileEvent, FileEventKind, FileWatcher};
