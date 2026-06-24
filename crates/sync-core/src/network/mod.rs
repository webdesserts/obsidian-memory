//! Iroh-based P2P networking for sync-core.
//!
//! This module implements the network layer using iroh (QUIC transport, hole punching,
//! relay servers) and iroh-gossip (HyParView + PlumTree epidemic broadcast).
//!
//! # Architecture
//!
//! ```text
//! SyncNode
//! ├── iroh::Endpoint      — QUIC transport with relay fallback
//! ├── iroh_gossip::Gossip — HyParView membership + PlumTree broadcast
//! ├── iroh::Router        — ALPN-based protocol dispatch
//! └── GossipHandle        — Per-vault gossip topic subscription
//! ```
//!
//! Two channels serve different purposes:
//! - **Gossip**: Lightweight change notifications (~1KB). Broadcast to all peers.
//! - **QUIC streams**: Heavy data transfer. Version vector exchange, bulk updates.
//!
//! # Platform support
//!
//! The networking stack compiles for both native and WASM targets.
//! On native, peers connect via direct QUIC sockets or relay-assisted hole punching.
//! In WASM (Obsidian plugin), peers connect exclusively via relay-assisted QUIC
//! since browsers cannot bind UDP sockets directly.
//!
//! mDNS local discovery requires OS-level networking and is native-only.

pub mod gossip;
pub mod streams;
pub use node::{SYNC_ALPN, SyncNode};
mod node;

/// The gossip protocol ALPN, re-exported so downstream crates (e.g. the daemon's
/// reconnect supervisor) can open gossip-bound connections without taking a
/// direct dependency on `iroh-gossip`.
pub use iroh_gossip::net::GOSSIP_ALPN;

// mDNS discovery types now live in `p2p-core` (native-only networking substrate).
// Re-exported here so existing `sync_core::network::discovery::*` paths keep
// resolving for the daemon and node/mesh_mdns code.
pub use p2p_core::discovery;

// pairing is native-only — it depends on iroh::protocol::ProtocolHandler
// and tokio::time::timeout which are unavailable in WASM.
#[cfg(feature = "native")]
pub mod mesh_mdns;
#[cfg(feature = "native")]
pub mod pairing;
