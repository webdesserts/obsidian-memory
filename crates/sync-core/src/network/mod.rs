//! The vault-sync wire protocol layered onto p2p-core's networking substrate.
//!
//! The generic node, gossip transport, mDNS discovery, relay, and pairing handler
//! all live in `p2p-core` (`P2pNode`). sync-core no longer names iroh anything — it
//! layers the *vault-sync specifics* back on top:
//!
//! # Architecture
//!
//! ```text
//! p2p_core::P2pNode          — QUIC endpoint + Router + gossip transport (the substrate)
//! └── sync-core (this module)
//!     ├── SyncNodeSeam       — binds SYNC_ALPN + the sync stream handler
//!     ├── VaultGossipExt     — maps VaultId onto the generic gossip topic
//!     ├── gossip::VaultGossip — the GossipMessage codec over the byte-transport
//!     └── streams            — the QUIC bi-stream sync request/response codec
//! ```
//!
//! Two channels serve different purposes:
//! - **Gossip**: Lightweight change notifications (~1KB). Broadcast to all peers.
//! - **QUIC streams**: Heavy data transfer. Version vector exchange, bulk updates.
//!
//! Native-only: the networking stack requires OS-level sockets (direct QUIC,
//! relay-assisted hole punching, mDNS), all of which live in p2p-core.

pub mod gossip;
pub mod node_seam;
pub mod streams;

/// ALPN identifier for our custom sync protocol.
///
/// iroh's Router dispatches incoming QUIC connections to the correct handler
/// based on ALPN. This ALPN routes to our QUIC stream sync handler. It is the
/// vault-sync wire contract — do NOT change the literal.
pub const SYNC_ALPN: &[u8] = b"obsidian-memory/sync/1";

// The iroh node now lives in `p2p-core` as `P2pNode`. Re-exported here as
// `SyncNode` so the daemon's `sync_core::network::SyncNode` field-type and
// param-type references resolve unchanged. The vault-sync constructors and the
// `VaultId`-typed gossip helpers are layered back on via the extension traits in
// `node_seam` ([`SyncNodeSeam`], [`VaultGossipExt`]); callers bring them into
// scope with `use sync_core::network::{SyncNodeSeam, VaultGossipExt};`.
pub use node_seam::{SyncNodeSeam, VaultGossipExt};
pub use p2p_core::P2pNode as SyncNode;

/// The gossip protocol ALPN, re-exported so downstream crates (e.g. the daemon's
/// reconnect supervisor) can open gossip-bound connections without taking a
/// direct dependency on `iroh-gossip`.
pub use p2p_core::GOSSIP_ALPN;

// mDNS discovery, the mDNS actor, and the pairing QUIC handler now live in
// `p2p-core` (native-only networking substrate). Re-exported here so existing
// `sync_core::network::{discovery, mesh_mdns, pairing}::*` paths keep resolving
// for the daemon and node code. The pairing handler is re-exported under the
// `pairing` alias (its p2p-core module is `pairing_handler`, distinct from the
// `pairing` message-types module).
pub use p2p_core::pairing_handler as pairing;
pub use p2p_core::{discovery, mesh_mdns};
