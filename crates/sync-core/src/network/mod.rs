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
//! # Feature flags
//!
//! The full networking stack requires the `native` feature (tokio runtime, real sockets).
//! WASM builds include iroh for compilation but cannot run the networking stack directly.

#[cfg(feature = "native")]
pub mod gossip;

#[cfg(feature = "native")]
pub mod streams;

// Discovery is native-only (mDNS requires OS-level networking)
#[cfg(feature = "native")]
pub mod discovery;

#[cfg(feature = "native")]
pub use node::{SyncNode, SYNC_ALPN};

#[cfg(feature = "native")]
mod node;
