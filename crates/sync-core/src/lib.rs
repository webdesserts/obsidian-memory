// Deny holding RefCell borrows across await points - causes WASM panics
#![deny(clippy::await_holding_refcell_ref)]

//! sync-core: Shared Rust library for the P2P vault sync wire protocol.
//!
//! This crate provides the networking surface the daemon runs on:
//! - The sync wire protocol between peers (`network`, `sync`)
//! - Peer pairing, allowlist, and roster management (`pairing`, `allowlist`, `peers`)
//! - Vault identity (`peer_id`) and the test-time-scale lever (`time_scale`)
//!
//! The legacy path-hash vault/document/markdown engine was removed at the
//! `vault-sync` (UUID-store) cutover; the live vault/fs/document layer now lives
//! in the `vault-sync` crate, not here.
//!
//! ## Feature gates
//!
//! - `cfg(feature = "native")` — code requiring iroh mDNS/sockets, tokio runtime, or
//!   OS-level APIs. These are unavailable in WASM or when building without the `native`
//!   feature (e.g. the Obsidian plugin via sync-wasm).
//! - `cfg(target_arch = "wasm32")` — WASM-specific runtime differences: `Rc`/`RefCell`
//!   instead of `Arc`/`RwLock`, and `?Send` async trait bounds. These gates exist
//!   because Cargo feature unification can't express "Send only when native feature is
//!   enabled" — the arch check is the only reliable way to vary trait bounds.

pub mod network;

pub mod pairing;
pub mod peer_id;

// `KeyStorage`/`PeerId`, the file-backed identity types, and the peer allowlist
// now live in `p2p-core` (the native networking substrate sync-core depends on).
// Re-export the whole modules so existing `sync_core::key_storage::{...}` and
// `sync_core::allowlist::{...}` paths keep resolving across the daemon — and so
// sync-core's own `crate::allowlist::*` imports in the `network` submodules keep
// resolving without edits.
pub use p2p_core::allowlist;
pub use p2p_core::key_storage;
pub mod peers;
pub mod sync;
pub mod time_scale;

pub use p2p_core::allowlist::{AllowedPeer, AllowlistError, AllowlistStorage, InMemoryAllowlist};
pub use p2p_core::key_storage::{KeyStorage, KeyStorageError};
pub use pairing::{
    PairingChallenge, PairingHello, PairingResponse, PairingResult, PairingSession, compute_hmac,
    generate_pairing_code, verify_hmac,
};
pub use peer_id::{PeerId, PeerIdError, VaultId};
pub use peers::{PeerEntry, PeerRegistry, PeerState};
pub use sync::SyncMessage;
