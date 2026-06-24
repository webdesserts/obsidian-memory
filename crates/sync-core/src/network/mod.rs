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

// mDNS discovery, the mDNS actor, and the pairing QUIC handler now live in
// `p2p-core` (native-only networking substrate). Re-exported here so existing
// `sync_core::network::{discovery, mesh_mdns, pairing}::*` paths keep resolving
// for the daemon and node code. The pairing handler is re-exported under the
// `pairing` alias (its p2p-core module is `pairing_handler`, distinct from the
// `pairing` message-types module).
pub use p2p_core::pairing_handler as pairing;
pub use p2p_core::{discovery, mesh_mdns};
