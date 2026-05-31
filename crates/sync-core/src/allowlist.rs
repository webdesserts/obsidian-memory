//! AllowlistStorage trait for managing authorized peers.
//!
//! Implementations:
//! - `FileAllowlistStorage` (in sync-daemon) — reads/writes `.sync/allowlist.json`
//! - Plugin implementation (future) — reads/writes via Obsidian's localStorage bridge

use async_trait::async_trait;
use serde::{Deserialize, Serialize};
use thiserror::Error;
use web_time::SystemTime;

use crate::peer_id::PeerId;

#[derive(Debug, Error)]
pub enum AllowlistError {
    #[error("Allowlist file is corrupt or invalid JSON")]
    InvalidData,

    #[error("IO error: {0}")]
    Io(String),
}

pub type Result<T> = std::result::Result<T, AllowlistError>;

/// An authorized peer stored in the allowlist.
///
/// Each entry represents a device that has been paired and is allowed to sync.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AllowedPeer {
    /// The peer's ed25519 public key (64-char hex).
    pub node_id: PeerId,
    /// Human-readable device name (e.g., "umbra", "MacBook Pro").
    pub device_name: String,
    /// When the peer was paired (Unix timestamp milliseconds).
    pub paired_at: u64,
    /// When we last saw this peer online (Unix timestamp milliseconds), if ever.
    pub last_seen: Option<u64>,
}

impl AllowedPeer {
    /// Create a new allowed peer entry with the current time as `paired_at`.
    pub fn new(node_id: PeerId, device_name: impl Into<String>) -> Self {
        let paired_at = SystemTime::now()
            .duration_since(web_time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0);

        Self {
            node_id,
            device_name: device_name.into(),
            paired_at,
            last_seen: None,
        }
    }
}

/// Abstracts loading and persisting the allowlist of authorized peers.
///
/// The allowlist controls which devices are permitted to sync with this vault.
/// Only peers whose `node_id` appears in the allowlist will be accepted.
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg(not(target_arch = "wasm32"))]
pub trait AllowlistStorage: Send + Sync {
    /// Load all allowed peers from storage.
    async fn list_peers(&self) -> Result<Vec<AllowedPeer>>;

    /// Save the full allowlist to storage.
    async fn save_peers(&self, peers: &[AllowedPeer]) -> Result<()>;

    /// Add a peer to the allowlist.
    ///
    /// If a peer with this `node_id` already exists, updates the device name.
    async fn add_peer(&self, node_id: PeerId, device_name: impl Into<String> + Send) -> Result<()> {
        let mut peers = self.list_peers().await?;
        let device_name = device_name.into();

        if let Some(existing) = peers.iter_mut().find(|p| p.node_id == node_id) {
            existing.device_name = device_name;
        } else {
            peers.push(AllowedPeer::new(node_id, device_name));
        }

        self.save_peers(&peers).await
    }

    /// Remove a peer from the allowlist.
    async fn remove_peer(&self, node_id: &PeerId) -> Result<()> {
        let mut peers = self.list_peers().await?;
        peers.retain(|p| &p.node_id != node_id);
        self.save_peers(&peers).await
    }

    /// Check whether a peer is authorized to connect.
    async fn is_allowed(&self, node_id: &PeerId) -> Result<bool> {
        let peers = self.list_peers().await?;
        Ok(peers.iter().any(|p| &p.node_id == node_id))
    }

    /// Update the last-seen timestamp for a peer.
    ///
    /// If the peer is not in the allowlist, this is a no-op.
    async fn update_last_seen(&self, node_id: &PeerId, timestamp_ms: u64) -> Result<()> {
        let mut peers = self.list_peers().await?;
        if let Some(peer) = peers.iter_mut().find(|p| &p.node_id == node_id) {
            peer.last_seen = Some(timestamp_ms);
            self.save_peers(&peers).await?;
        }
        Ok(())
    }
}

/// WASM version of the AllowlistStorage trait (without Send + Sync bounds).
#[cfg_attr(target_arch = "wasm32", async_trait(?Send))]
#[cfg_attr(not(target_arch = "wasm32"), async_trait)]
#[cfg(target_arch = "wasm32")]
pub trait AllowlistStorage {
    /// Load all allowed peers from storage.
    async fn list_peers(&self) -> Result<Vec<AllowedPeer>>;

    /// Save the full allowlist to storage.
    async fn save_peers(&self, peers: &[AllowedPeer]) -> Result<()>;

    /// Add a peer to the allowlist.
    ///
    /// If a peer with this `node_id` already exists, updates the device name.
    async fn add_peer(&self, node_id: PeerId, device_name: impl Into<String>) -> Result<()> {
        let mut peers = self.list_peers().await?;
        let device_name = device_name.into();

        if let Some(existing) = peers.iter_mut().find(|p| p.node_id == node_id) {
            existing.device_name = device_name;
        } else {
            peers.push(AllowedPeer::new(node_id, device_name));
        }

        self.save_peers(&peers).await
    }

    /// Remove a peer from the allowlist.
    async fn remove_peer(&self, node_id: &PeerId) -> Result<()> {
        let mut peers = self.list_peers().await?;
        peers.retain(|p| &p.node_id != node_id);
        self.save_peers(&peers).await
    }

    /// Check whether a peer is authorized to connect.
    async fn is_allowed(&self, node_id: &PeerId) -> Result<bool> {
        let peers = self.list_peers().await?;
        Ok(peers.iter().any(|p| &p.node_id == node_id))
    }

    /// Update the last-seen timestamp for a peer.
    ///
    /// If the peer is not in the allowlist, this is a no-op.
    async fn update_last_seen(&self, node_id: &PeerId, timestamp_ms: u64) -> Result<()> {
        let mut peers = self.list_peers().await?;
        if let Some(peer) = peers.iter_mut().find(|p| &p.node_id == node_id) {
            peer.last_seen = Some(timestamp_ms);
            self.save_peers(&peers).await?;
        }
        Ok(())
    }
}

/// In-memory implementation of `AllowlistStorage` for testing.
///
/// Thread-safe, zero-I/O — suitable for integration tests that need
/// a real `AllowlistStorage` without touching the filesystem.
#[cfg(not(target_arch = "wasm32"))]
pub struct InMemoryAllowlist {
    peers: std::sync::RwLock<Vec<AllowedPeer>>,
}

#[cfg(not(target_arch = "wasm32"))]
impl std::fmt::Debug for InMemoryAllowlist {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("InMemoryAllowlist").finish()
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl InMemoryAllowlist {
    /// Create a new empty in-memory allowlist.
    pub fn new() -> Self {
        Self {
            peers: std::sync::RwLock::new(Vec::new()),
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
impl Default for InMemoryAllowlist {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(not(target_arch = "wasm32"))]
#[async_trait]
impl AllowlistStorage for InMemoryAllowlist {
    async fn list_peers(&self) -> Result<Vec<AllowedPeer>> {
        Ok(self.peers.read().unwrap().clone())
    }

    async fn save_peers(&self, peers: &[AllowedPeer]) -> Result<()> {
        *self.peers.write().unwrap() = peers.to_vec();
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn peer_a() -> PeerId {
        PeerId::generate()
    }

    fn peer_b() -> PeerId {
        PeerId::generate()
    }

    #[tokio::test]
    async fn test_list_peers_empty() {
        let storage = InMemoryAllowlist::new();
        let peers = storage.list_peers().await.unwrap();
        assert!(peers.is_empty());
    }

    #[tokio::test]
    async fn test_add_and_list_peer() {
        let storage = InMemoryAllowlist::new();
        let id = peer_a();

        storage.add_peer(id, "umbra").await.unwrap();

        let peers = storage.list_peers().await.unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].node_id, id);
        assert_eq!(peers[0].device_name, "umbra");
        assert!(peers[0].last_seen.is_none());
    }

    #[tokio::test]
    async fn test_add_peer_updates_existing() {
        let storage = InMemoryAllowlist::new();
        let id = peer_a();

        storage.add_peer(id, "old-name").await.unwrap();
        storage.add_peer(id, "new-name").await.unwrap();

        let peers = storage.list_peers().await.unwrap();
        assert_eq!(peers.len(), 1, "should not duplicate");
        assert_eq!(peers[0].device_name, "new-name");
    }

    #[tokio::test]
    async fn test_remove_peer() {
        let storage = InMemoryAllowlist::new();
        let a = peer_a();
        let b = peer_b();

        storage.add_peer(a, "device-a").await.unwrap();
        storage.add_peer(b, "device-b").await.unwrap();
        storage.remove_peer(&a).await.unwrap();

        let peers = storage.list_peers().await.unwrap();
        assert_eq!(peers.len(), 1);
        assert_eq!(peers[0].node_id, b);
    }

    #[tokio::test]
    async fn test_is_allowed() {
        let storage = InMemoryAllowlist::new();
        let a = peer_a();
        let b = peer_b();

        storage.add_peer(a, "device-a").await.unwrap();

        assert!(storage.is_allowed(&a).await.unwrap());
        assert!(!storage.is_allowed(&b).await.unwrap());
    }

    #[tokio::test]
    async fn test_update_last_seen() {
        let storage = InMemoryAllowlist::new();
        let id = peer_a();

        storage.add_peer(id, "umbra").await.unwrap();
        storage
            .update_last_seen(&id, 1_700_000_000_000)
            .await
            .unwrap();

        let peers = storage.list_peers().await.unwrap();
        assert_eq!(peers[0].last_seen, Some(1_700_000_000_000));
    }

    #[tokio::test]
    async fn test_update_last_seen_noop_for_unknown() {
        let storage = InMemoryAllowlist::new();
        let unknown = peer_a();

        // Should not error — just silently ignore unknown peers
        storage.update_last_seen(&unknown, 1_000).await.unwrap();

        let peers = storage.list_peers().await.unwrap();
        assert!(peers.is_empty());
    }
}
