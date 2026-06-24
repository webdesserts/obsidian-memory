//! sync-daemon library: Exposes internal modules for testing.
//!
//! This is a thin library layer over the daemon components,
//! allowing integration tests to access internal types.

pub mod allowlist;
pub mod daemon;
pub mod daemon_lock;
pub mod http;
pub mod move_coalescer;
pub mod native_fs;
pub mod pair;
pub mod pair_api;
pub mod pair_shared;
pub mod persistence;
pub mod relay;
pub mod relay_class;
pub mod watcher;

// Re-export key types for convenience
pub use allowlist::FileAllowlistStorage;
pub use native_fs::NativeFs;
// FileKeyStorage / IdentityKey now live in p2p-core (the native networking substrate).
pub use p2p_core::{FileKeyStorage, IdentityKey};
pub use watcher::{FileEvent, FileEventKind, FileWatcher};
