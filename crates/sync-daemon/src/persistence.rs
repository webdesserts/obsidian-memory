//! Persistence for known peers.
//!
//! Stores peer information to disk for recovery after restarts.
//! Peers are stored in `.sync/known_peers.json` within the vault directory.

use anyhow::Result;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use sync_core::peer_id::PeerId;
use sync_core::swim::PeerInfo;

/// Persisted peer information.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PersistedPeer {
    /// Peer ID
    pub peer_id: String,
    /// Address for connecting (None for client-only peers)
    pub address: Option<String>,
    /// Last time we were connected (unix timestamp ms)
    pub last_seen: u64,
    /// Peer ID of who told us about this peer (for debugging)
    pub discovered_via: Option<String>,
}

impl PersistedPeer {
    /// Create from PeerInfo and current time.
    pub fn from_peer_info(info: &PeerInfo, now_ms: u64, discovered_via: Option<PeerId>) -> Self {
        Self {
            peer_id: info.peer_id.to_string(),
            address: info.address.clone(),
            last_seen: now_ms,
            discovered_via: discovered_via.map(|p| p.to_string()),
        }
    }

    /// Convert back to PeerInfo.
    pub fn to_peer_info(&self) -> Result<PeerInfo> {
        let peer_id: PeerId = self.peer_id.parse()?;
        Ok(PeerInfo {
            peer_id,
            address: self.address.clone(),
        })
    }
}

/// Collection of persisted peers.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PersistedPeers {
    /// All known peers.
    pub peers: Vec<PersistedPeer>,
}

impl PersistedPeers {
    /// Create an empty collection.
    pub fn new() -> Self {
        Self { peers: Vec::new() }
    }

    /// Add or update a peer.
    pub fn upsert(&mut self, peer: PersistedPeer) {
        if let Some(existing) = self.peers.iter_mut().find(|p| p.peer_id == peer.peer_id) {
            // Update existing peer
            existing.address = peer.address;
            existing.last_seen = peer.last_seen;
            if peer.discovered_via.is_some() {
                existing.discovered_via = peer.discovered_via;
            }
        } else {
            // Add new peer
            self.peers.push(peer);
        }
    }

    /// Remove a peer by ID.
    pub fn remove(&mut self, peer_id: &str) {
        self.peers.retain(|p| p.peer_id != peer_id);
    }

    /// Get a peer by ID.
    pub fn get(&self, peer_id: &str) -> Option<&PersistedPeer> {
        self.peers.iter().find(|p| p.peer_id == peer_id)
    }

    /// Get all peers with addresses (can be reconnected to).
    pub fn reconnectable(&self) -> impl Iterator<Item = &PersistedPeer> {
        self.peers.iter().filter(|p| p.address.is_some())
    }
}

/// Storage for persisted peers.
pub struct PeerStorage {
    /// Path to the storage file.
    path: PathBuf,
    /// In-memory cache.
    peers: PersistedPeers,
}

impl PeerStorage {
    /// Create storage at the specified vault directory.
    ///
    /// Creates `.sync/known_peers.json` within the vault.
    pub fn new(vault_path: &Path) -> Result<Self> {
        let sync_dir = vault_path.join(".sync");
        let path = sync_dir.join("known_peers.json");

        let mut storage = Self {
            path,
            peers: PersistedPeers::new(),
        };

        // Try to load existing data
        if let Ok(loaded) = storage.load() {
            storage.peers = loaded;
        }

        Ok(storage)
    }

    /// Load peers from disk.
    pub fn load(&self) -> Result<PersistedPeers> {
        if !self.path.exists() {
            return Ok(PersistedPeers::new());
        }

        let contents = fs::read_to_string(&self.path)?;
        let peers: PersistedPeers = serde_json::from_str(&contents)?;
        Ok(peers)
    }

    /// Save current peers to disk.
    pub fn save(&self) -> Result<()> {
        // Ensure directory exists
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }

        let contents = serde_json::to_string_pretty(&self.peers)?;
        fs::write(&self.path, contents)?;
        Ok(())
    }

    /// Add or update a peer.
    pub fn upsert(&mut self, peer: PersistedPeer) -> Result<()> {
        self.peers.upsert(peer);
        self.save()
    }

    /// Remove a peer.
    pub fn remove(&mut self, peer_id: &str) -> Result<()> {
        self.peers.remove(peer_id);
        self.save()
    }

    /// Get a peer by ID.
    pub fn get(&self, peer_id: &str) -> Option<&PersistedPeer> {
        self.peers.get(peer_id)
    }

    /// Get all reconnectable peers.
    pub fn reconnectable(&self) -> impl Iterator<Item = &PersistedPeer> {
        self.peers.reconnectable()
    }

    /// Get all peers.
    pub fn all(&self) -> &[PersistedPeer] {
        &self.peers.peers
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn test_peer_a() -> PersistedPeer {
        PersistedPeer {
            peer_id: "a".repeat(16),
            address: Some("ws://a:8080".into()),
            last_seen: 1000,
            discovered_via: None,
        }
    }

    fn test_peer_b() -> PersistedPeer {
        PersistedPeer {
            peer_id: "b".repeat(16),
            address: Some("ws://b:8080".into()),
            last_seen: 2000,
            discovered_via: Some("a".repeat(16)),
        }
    }

    fn test_client_peer() -> PersistedPeer {
        PersistedPeer {
            peer_id: "c".repeat(16),
            address: None,
            last_seen: 3000,
            discovered_via: Some("a".repeat(16)),
        }
    }

    // ==================== PersistedPeers tests ====================

    #[test]
    fn test_persisted_peers_upsert_new() {
        let mut peers = PersistedPeers::new();

        peers.upsert(test_peer_a());

        assert_eq!(peers.peers.len(), 1);
        assert_eq!(peers.get(&"a".repeat(16)).unwrap().address.as_deref(), Some("ws://a:8080"));
    }

    #[test]
    fn test_persisted_peers_upsert_update() {
        let mut peers = PersistedPeers::new();

        peers.upsert(test_peer_a());

        // Update with new address
        let updated = PersistedPeer {
            peer_id: "a".repeat(16),
            address: Some("ws://a-new:8080".into()),
            last_seen: 2000,
            discovered_via: None,
        };
        peers.upsert(updated);

        assert_eq!(peers.peers.len(), 1);
        assert_eq!(peers.get(&"a".repeat(16)).unwrap().address.as_deref(), Some("ws://a-new:8080"));
        assert_eq!(peers.get(&"a".repeat(16)).unwrap().last_seen, 2000);
    }

    #[test]
    fn test_persisted_peers_remove() {
        let mut peers = PersistedPeers::new();

        peers.upsert(test_peer_a());
        peers.upsert(test_peer_b());
        peers.remove(&"a".repeat(16));

        assert_eq!(peers.peers.len(), 1);
        assert!(peers.get(&"a".repeat(16)).is_none());
        assert!(peers.get(&"b".repeat(16)).is_some());
    }

    #[test]
    fn test_persisted_peers_reconnectable() {
        let mut peers = PersistedPeers::new();

        peers.upsert(test_peer_a());
        peers.upsert(test_peer_b());
        peers.upsert(test_client_peer());

        let reconnectable: Vec<_> = peers.reconnectable().collect();

        // Should only include peers with addresses
        assert_eq!(reconnectable.len(), 2);
        assert!(reconnectable.iter().all(|p| p.address.is_some()));
    }

    // ==================== PeerStorage tests ====================

    #[test]
    fn test_persist_known_peers() {
        let temp_dir = TempDir::new().unwrap();
        let vault_path = temp_dir.path();

        {
            let mut storage = PeerStorage::new(vault_path).unwrap();
            storage.upsert(test_peer_a()).unwrap();
            storage.upsert(test_peer_b()).unwrap();
        }

        // File should exist
        let peer_file = vault_path.join(".sync/known_peers.json");
        assert!(peer_file.exists());

        // Should be valid JSON
        let contents = fs::read_to_string(&peer_file).unwrap();
        let loaded: PersistedPeers = serde_json::from_str(&contents).unwrap();
        assert_eq!(loaded.peers.len(), 2);
    }

    #[test]
    fn test_load_persisted_peers() {
        let temp_dir = TempDir::new().unwrap();
        let vault_path = temp_dir.path();

        // First session: save peers
        {
            let mut storage = PeerStorage::new(vault_path).unwrap();
            storage.upsert(test_peer_a()).unwrap();
            storage.upsert(test_peer_b()).unwrap();
        }

        // Second session: load peers
        {
            let storage = PeerStorage::new(vault_path).unwrap();
            let all: Vec<_> = storage.all().to_vec();

            assert_eq!(all.len(), 2);
            assert!(storage.get(&"a".repeat(16)).is_some());
            assert!(storage.get(&"b".repeat(16)).is_some());
        }
    }

    #[test]
    fn test_persist_incoming_connections() {
        let temp_dir = TempDir::new().unwrap();
        let vault_path = temp_dir.path();

        // First server: receives incoming connection from client-only peer
        let mut storage = PeerStorage::new(vault_path).unwrap();

        // Even client-only peers should be persisted (so we know about them)
        storage.upsert(test_client_peer()).unwrap();

        // Server peer that connected to us should be persisted with their address
        let server_peer = PersistedPeer {
            peer_id: "d".repeat(16),
            address: Some("ws://d:8080".into()),
            last_seen: 4000,
            discovered_via: None,
        };
        storage.upsert(server_peer).unwrap();

        // Verify both are saved
        let storage2 = PeerStorage::new(vault_path).unwrap();
        assert_eq!(storage2.all().len(), 2);

        // Reconnectable should only return the server peer
        let reconnectable: Vec<_> = storage2.reconnectable().collect();
        assert_eq!(reconnectable.len(), 1);
        assert_eq!(reconnectable[0].peer_id, "d".repeat(16));
    }

    #[test]
    fn test_offline_rejoin() {
        let temp_dir = TempDir::new().unwrap();
        let vault_path = temp_dir.path();

        // First session: peer C knows about A and B
        {
            let mut storage = PeerStorage::new(vault_path).unwrap();
            storage.upsert(test_peer_a()).unwrap();
            storage.upsert(test_peer_b()).unwrap();
        }

        // Simulate offline period (30 days)...
        // When coming back online, peers should be loadable

        {
            let storage = PeerStorage::new(vault_path).unwrap();
            let reconnectable: Vec<_> = storage.reconnectable().collect();

            // Should still have A and B addresses to reconnect to
            assert_eq!(reconnectable.len(), 2);
            assert!(reconnectable.iter().any(|p| p.address.as_deref() == Some("ws://a:8080")));
            assert!(reconnectable.iter().any(|p| p.address.as_deref() == Some("ws://b:8080")));
        }
    }

    #[test]
    fn test_first_server_restart() {
        let temp_dir = TempDir::new().unwrap();
        let vault_path = temp_dir.path();

        // First server A: receives incoming connections from B and C
        {
            let mut storage = PeerStorage::new(vault_path).unwrap();

            // Incoming connection from server B
            let server_b = PersistedPeer {
                peer_id: "b".repeat(16),
                address: Some("ws://b:8080".into()),
                last_seen: 1000,
                discovered_via: None, // Direct connection
            };
            storage.upsert(server_b).unwrap();

            // Incoming connection from client-only C
            storage.upsert(test_client_peer()).unwrap();
        }

        // Server A restarts
        {
            let storage = PeerStorage::new(vault_path).unwrap();
            let reconnectable: Vec<_> = storage.reconnectable().collect();

            // Should try to reconnect to B (has address)
            assert_eq!(reconnectable.len(), 1);
            assert_eq!(reconnectable[0].peer_id, "b".repeat(16));
            assert_eq!(reconnectable[0].address.as_deref(), Some("ws://b:8080"));

            // Client-only C should still be known but not reconnectable
            assert!(storage.get(&"c".repeat(16)).is_some());
        }
    }
}
