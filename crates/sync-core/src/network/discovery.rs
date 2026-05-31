//! Local network mesh discovery via mDNS.
//!
//! Devices advertise their mesh (vault) on the LAN using iroh's `MdnsAddressLookup`
//! with a custom service name `obsidian-sync`. Devices sharing the same `VaultId`
//! form a single mesh — this is the foundation for the pairing flow (Item 5).
//!
//! mDNS is native-only (requires OS-level networking) and is gated by the
//! `address-lookup-mdns` feature via the `native` feature set.

use iroh::EndpointId;
use serde::{Deserialize, Serialize};

/// Metadata broadcast via mDNS to identify a mesh (vault).
///
/// Serialized as compact JSON and carried in the mDNS `UserData` TXT record.
/// The total serialized size must stay well under 245 bytes (the `UserData` limit).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MeshMetadata {
    /// Human-readable mesh name (e.g., "Michael's Notes").
    pub mesh: String,
    /// VaultId hex string — groups devices into the same mesh.
    pub vid: String,
    /// Protocol version for forward compatibility.
    pub ver: u32,
}

/// A discovered mesh on the local network.
///
/// Aggregates all devices that share the same `VaultId` into one mesh entry.
/// Built from `DiscoveryEvent::Discovered` events by the caller.
#[derive(Debug, Clone)]
pub struct DiscoveredMesh {
    /// Mesh name from the metadata.
    pub mesh_name: String,
    /// VaultId for grouping (hex string).
    pub vault_id: String,
    /// Discovered peers in this mesh (their EndpointIds).
    pub peers: Vec<EndpointId>,
    /// Number of online devices (equal to `peers.len()` at discovery time).
    pub online_count: usize,
}
