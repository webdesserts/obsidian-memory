// Deny holding RefCell borrows across await points - causes WASM panics
#![deny(clippy::await_holding_refcell_ref)]

//! vault-sync: the loro-half of P2P vault synchronization.
//!
//! This crate owns the vault data model and sync protocol — the Loro CRDT layer
//! that turns a folder of markdown notes into a convergent, peer-syncable vault.
//! It knows nothing about transport (iroh, sockets, relays); a consumer (the
//! sync daemon) wires it to a network and drives it through the byte seam.
//!
//! ## Feature gates
//!
//! - `cfg(target_arch = "wasm32")` — WASM-specific runtime differences: `Rc`/`RefCell`
//!   instead of `Arc`/`RwLock`, and `?Send` async trait bounds. These gates exist
//!   because Cargo feature unification can't express "Send only when native feature is
//!   enabled" — the arch check is the only reliable way to vary trait bounds.

pub mod fs;

pub use fs::{FileEntry, FileStat, FileSystem, InMemoryFs};
