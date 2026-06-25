// Deny holding RefCell borrows across await points - causes WASM panics
#![deny(clippy::await_holding_refcell_ref)]

//! sync-core: Shared Rust library for the P2P vault sync wire protocol.
//!
//! This crate provides the networking surface the daemon runs on:
//! - The sync wire protocol between peers (`network`, `sync`)
//! - Peer pairing, allowlist, and roster management (`pairing`, `allowlist` re-exported from `p2p-core`; `peers` local)
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

pub mod peer_id;

// `KeyStorage`/`PeerId`, the file-backed identity types, the peer allowlist, and
// the pairing protocol now live in `p2p-core` (the native networking substrate
// sync-core depends on). Re-export the whole modules so existing
// `sync_core::key_storage::{...}`, `sync_core::allowlist::{...}`, and
// `sync_core::pairing::{...}` paths keep resolving across the daemon — and so
// sync-core's own `crate::allowlist::*` / `crate::pairing::*` imports in the
// `network` submodules keep resolving without edits.
pub use p2p_core::allowlist;
pub use p2p_core::key_storage;
pub use p2p_core::pairing;
pub mod peers;
pub mod sync;
// The process-global test-time-scale lever now lives in `p2p-core` (the lowest
// layer of the stack, no deps). Re-export the whole module so existing
// `sync_core::time_scale::*` and the network submodules' `crate::time_scale::*`
// paths keep resolving, and so the single `OnceLock` has exactly one home that
// the daemon's lone `set_time_scale` seed reaches.
pub use p2p_core::time_scale;

pub use p2p_core::allowlist::{AllowedPeer, AllowlistError, AllowlistStorage, InMemoryAllowlist};
pub use p2p_core::key_storage::{KeyStorage, KeyStorageError};
pub use p2p_core::pairing::{
    PairingChallenge, PairingHello, PairingResponse, PairingResult, PairingSession, compute_hmac,
    generate_pairing_code, verify_hmac,
};
pub use peer_id::{PeerId, PeerIdError, VaultId};
pub use peers::{ConnectTransition, PeerEntry, PeerRegistry, PeerState};
pub use sync::SyncMessage;
