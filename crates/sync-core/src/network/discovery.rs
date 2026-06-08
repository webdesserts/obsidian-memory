//! Local network mesh discovery via mDNS.
//!
//! Devices advertise their mesh (vault) on the LAN using `MeshMdns` — a V4-only
//! swarm-discovery wrapper — with a custom service name `obsidian-sync`. Devices
//! sharing the same `VaultId` form a single mesh — this is the foundation for the
//! pairing flow.
//!
//! mDNS is native-only (requires OS-level networking) and is gated by the `native`
//! feature set.

use iroh::EndpointId;
use iroh::address_lookup::UserData;
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

/// Data about a discovered endpoint — analogous to `iroh_mdns_address_lookup`'s
/// `EndpointData` but lives here so daemon code can import it without depending on
/// the upstream crate.
#[derive(Debug, Clone)]
pub struct EndpointData {
    user_data: Option<UserData>,
}

impl EndpointData {
    pub fn new(user_data: Option<UserData>) -> Self {
        Self { user_data }
    }

    /// Returns the optional user-defined data of the endpoint.
    pub fn user_data(&self) -> Option<&UserData> {
        self.user_data.as_ref()
    }
}

/// An endpoint's ID paired with its discovery data.
#[derive(Debug, Clone)]
pub struct EndpointInfo {
    pub endpoint_id: EndpointId,
    pub data: EndpointData,
}

/// An event from the mDNS discovery stream.
#[derive(Debug, Clone)]
pub enum DiscoveryEvent {
    /// A new or updated peer was discovered.
    Discovered { endpoint_info: EndpointInfo },
    /// A previously-discovered peer has expired off the LAN.
    Expired { endpoint_id: EndpointId },
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
