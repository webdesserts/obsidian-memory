//! Persistence for daemon state and known peers.
//!
//! - **`IdentityKey`** (`.sync/daemon.key`): Ed25519 secret key for the daemon's network identity
//! - **`DaemonConfig`** (`.sync/daemon.toml`): Daemon config including the derived PeerId
//! - **`PeerStorage`** (`.sync/known_peers.json`): Known peers for recovery after restarts

use anyhow::{Context, Result};
use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::{Path, PathBuf};
use sync_core::key_storage::KeyStorage;
use sync_core::peer_id::PeerId;
use tracing::{info, warn};

use crate::key_storage::FileKeyStorage;

const DAEMON_KEY_FILE: &str = ".sync/daemon.key";
const DAEMON_CONFIG_FILE: &str = ".sync/daemon.toml";

/// The daemon's ed25519 secret key, persisted in `.sync/daemon.key`.
///
/// The public key (derived from this secret) is used as the daemon's `PeerId`.
/// On first run, a new keypair is generated and the secret key is written to disk.
/// On subsequent runs, the secret key is loaded to reconstruct a stable identity.
pub struct IdentityKey {
    signing_key: SigningKey,
}

impl IdentityKey {
    /// Load or generate the default identity key at `.sync/daemon.key`.
    ///
    /// Delegates I/O to `FileKeyStorage`, which handles file creation,
    /// permissions, and atomic loading.
    pub async fn load_or_generate(vault_path: &Path) -> Result<Self> {
        let storage = FileKeyStorage::new(vault_path);
        let bytes = storage
            .load_or_generate()
            .await
            .map_err(|e| anyhow::anyhow!("Key storage error: {}", e))?;
        let key = Self::from_bytes(bytes);
        info!(peer_id = %key.peer_id(), "Loaded or generated daemon identity key");
        Ok(key)
    }

    /// Load an identity key from a custom key file path.
    ///
    /// The file must contain exactly 32 bytes (the ed25519 secret key).
    pub fn load_from(key_path: &Path) -> Result<Self> {
        let bytes = fs::read(key_path)
            .with_context(|| format!("Failed to read key file: {}", key_path.display()))?;
        let bytes: [u8; 32] = bytes
            .try_into()
            .map_err(|_| anyhow::anyhow!("Key file must be exactly 32 bytes: {}", key_path.display()))?;
        Ok(Self::from_bytes(bytes))
    }

    /// Build an `IdentityKey` from raw secret key bytes.
    fn from_bytes(bytes: [u8; 32]) -> Self {
        Self {
            signing_key: SigningKey::from_bytes(&bytes),
        }
    }

    /// Derive the `PeerId` from this identity key's public key.
    pub fn peer_id(&self) -> PeerId {
        PeerId::from_bytes(self.signing_key.verifying_key().to_bytes())
    }

    /// Return the raw 32-byte ed25519 secret key.
    ///
    /// Used to initialize the iroh `SyncNode`, which needs the secret key to
    /// derive a stable `EndpointId` (the iroh equivalent of our `PeerId`).
    pub fn secret_key_bytes(&self) -> [u8; 32] {
        self.signing_key.to_bytes()
    }
}

/// Daemon-specific configuration persisted in `.sync/daemon.toml`.
///
/// Contains the daemon's PeerId (derived from the identity key). Separate from
/// vault-level metadata (VaultId in `metadata.toml`).
///
/// On upgrade from old u64 PeerIds, the old value is saved as `legacy_peer_id`
/// for reference. New peer_id is the 64-char hex ed25519 pubkey.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DaemonConfig {
    /// Daemon's network identity (64-char hex ed25519 pubkey)
    pub peer_id: PeerId,
    /// The old u64-based PeerId from before the ed25519 migration, if any.
    /// Stored for reference only — orphaned Loro version-vector entries are harmless.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub legacy_peer_id: Option<String>,
    /// URL of the embedded relay server, written on startup and cleared on shutdown.
    ///
    /// Plugin peers read this to discover the daemon's relay and route through it.
    /// Absent when the relay is not running (relay_listen was not set).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub relay_url: Option<String>,
}

impl DaemonConfig {
    /// Load or generate daemon config, deriving PeerId from the identity key.
    ///
    /// Returns `(config, identity_key)` so the caller can access the raw secret
    /// key bytes needed to initialize the iroh `SyncNode`.
    ///
    /// `identity_key_path` — if provided, loads a custom key file instead of the
    /// default `.sync/daemon.key`. This replaces the old `--peer-id` flag.
    pub async fn load_or_generate(vault_path: &Path, identity_key_path: Option<&Path>) -> Result<(Self, IdentityKey)> {
        let config_path = vault_path.join(DAEMON_CONFIG_FILE);
        let key_path = vault_path.join(DAEMON_KEY_FILE);

        // Detect legacy upgrade: daemon.toml exists but daemon.key doesn't.
        // This means the daemon was previously running with the old u64 PeerId format.
        let is_legacy_upgrade = config_path.exists() && !key_path.exists() && identity_key_path.is_none();

        // Load the identity key (custom path, or generate/load default)
        let identity_key = match identity_key_path {
            Some(path) => {
                let key = IdentityKey::load_from(path)?;
                info!(peer_id = %key.peer_id(), key_path = %path.display(), "Loaded custom identity key");
                key
            }
            None => IdentityKey::load_or_generate(vault_path).await?,
        };

        let new_peer_id = identity_key.peer_id();

        let config = if config_path.exists() {
            let contents = fs::read_to_string(&config_path)?;
            // Parse with optional incarnation field for migration from older config files
            let mut existing: DaemonConfigRaw = toml::from_str(&contents)
                .map_err(|e| anyhow::anyhow!("Corrupt daemon.toml: {}", e))?;

            if is_legacy_upgrade {
                // Read the raw peer_id string from TOML to capture the old 16-char value
                // before PeerId::from_str converts it to 64-char representation.
                let raw: toml::Value = toml::from_str(&contents)
                    .map_err(|e| anyhow::anyhow!("Corrupt daemon.toml: {}", e))?;
                let old_peer_id_str = raw
                    .get("peer_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();

                warn!(
                    old_peer_id = %old_peer_id_str,
                    new_peer_id = %new_peer_id,
                    "Upgrading from legacy u64 PeerId to ed25519 PeerId — clearing known_peers.json"
                );
                existing.legacy_peer_id = Some(old_peer_id_str);
                existing.peer_id = new_peer_id;
                clear_known_peers(vault_path);
            } else if existing.peer_id != new_peer_id {
                // Key file was swapped (e.g., --identity-key flag)
                info!(old = %existing.peer_id, new = %new_peer_id, "Identity key changed, updating PeerId");
                existing.peer_id = new_peer_id;
            }

            let config = DaemonConfig {
                peer_id: existing.peer_id,
                legacy_peer_id: existing.legacy_peer_id,
                // Relay URL is runtime state — always start with None and let the
                // daemon write it after the relay starts. A stale URL left from a
                // previous (crashed) run is meaningless after restart.
                relay_url: None,
            };
            info!(peer_id = %config.peer_id, "Loaded daemon config");
            config
        } else {
            let config = DaemonConfig {
                peer_id: new_peer_id,
                legacy_peer_id: None,
                relay_url: None,
            };
            info!(peer_id = %config.peer_id, "Generated new daemon config");
            config
        };

        // Always save (creates file on first run, applies migrations)
        config.save(vault_path)?;

        Ok((config, identity_key))
    }

    /// Write the relay URL to daemon.toml.
    ///
    /// Called after the embedded relay starts so plugin peers can discover it.
    /// Pass `None` to clear (written on daemon shutdown).
    pub fn set_relay_url(&mut self, url: Option<String>, vault_path: &Path) -> Result<()> {
        self.relay_url = url;
        self.save(vault_path)
    }

    /// Save the current config to `.sync/daemon.toml`.
    pub fn save(&self, vault_path: &Path) -> Result<()> {
        let config_path = vault_path.join(DAEMON_CONFIG_FILE);
        if let Some(parent) = config_path.parent() {
            fs::create_dir_all(parent)?;
        }
        let contents = toml::to_string(self)?;
        fs::write(&config_path, contents)?;
        Ok(())
    }
}

/// Raw deserialization type for migration — accepts optional `incarnation`,
/// `legacy_peer_id`, and `relay_url` fields from older `daemon.toml` files without failing.
#[derive(Deserialize)]
struct DaemonConfigRaw {
    peer_id: PeerId,
    legacy_peer_id: Option<String>,
    relay_url: Option<String>,
    #[allow(dead_code)]
    incarnation: Option<u64>,
}

/// Clear `known_peers.json` after a PeerId migration.
///
/// Old WebSocket addresses are still valid, but peer entries are keyed by old
/// PeerIds that no longer match. Clearing is simpler than re-keying.
fn clear_known_peers(vault_path: &Path) {
    let peers_path = vault_path.join(".sync/known_peers.json");
    if peers_path.exists() {
        if let Err(e) = fs::remove_file(&peers_path) {
            warn!("Failed to clear known_peers.json during migration: {}", e);
        } else {
            info!("Cleared known_peers.json after PeerId migration");
        }
    }
}

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

    // ==================== IdentityKey tests ====================

    #[tokio::test]
    async fn test_identity_key_generates_and_saves() {
        let temp_dir = TempDir::new().unwrap();
        let vault_path = temp_dir.path();

        let key = IdentityKey::load_or_generate(vault_path).await.unwrap();
        let peer_id = key.peer_id();

        // Key file should exist with 32 bytes
        let key_path = vault_path.join(".sync/daemon.key");
        assert!(key_path.exists());
        assert_eq!(fs::read(&key_path).unwrap().len(), 32);

        // PeerId should be valid (64-char hex)
        assert_eq!(peer_id.to_string().len(), 64);
        assert_ne!(peer_id.as_u64(), 0);
    }

    #[tokio::test]
    async fn test_identity_key_loads_and_matches() {
        let temp_dir = TempDir::new().unwrap();
        let vault_path = temp_dir.path();

        let key1 = IdentityKey::load_or_generate(vault_path).await.unwrap();
        let peer_id1 = key1.peer_id();

        // Second call should load the same key from disk
        let key2 = IdentityKey::load_or_generate(vault_path).await.unwrap();
        let peer_id2 = key2.peer_id();

        // Should produce the same PeerId after loading
        assert_eq!(peer_id1, peer_id2);
    }

    #[tokio::test]
    async fn test_identity_key_load_from_custom_path() {
        let temp_dir = TempDir::new().unwrap();
        let vault_path = temp_dir.path();

        // Generate a key in a separate location via load_or_generate
        let alt_dir = TempDir::new().unwrap();
        let alt_key = IdentityKey::load_or_generate(alt_dir.path()).await.unwrap();
        let alt_key_path = alt_dir.path().join(".sync/daemon.key");
        let alt_peer_id = alt_key.peer_id();

        // Load via custom path
        let loaded = IdentityKey::load_from(&alt_key_path).unwrap();
        assert_eq!(loaded.peer_id(), alt_peer_id);

        // Should differ from what a default generate would produce
        let default_key = IdentityKey::load_or_generate(vault_path).await.unwrap();
        assert_ne!(default_key.peer_id(), alt_peer_id);
    }

    #[tokio::test]
    async fn test_identity_key_secret_key_bytes() {
        let temp_dir = TempDir::new().unwrap();
        let vault_path = temp_dir.path();

        let key = IdentityKey::load_or_generate(vault_path).await.unwrap();
        let bytes = key.secret_key_bytes();

        // Verify round-trip: reconstructing from bytes produces the same PeerId
        let reconstructed = IdentityKey::from_bytes(bytes);
        assert_eq!(reconstructed.peer_id(), key.peer_id());
    }

    // ==================== DaemonConfig tests ====================

    #[tokio::test]
    async fn test_daemon_config_generates_new() {
        let temp_dir = TempDir::new().unwrap();
        let vault_path = temp_dir.path();

        let (config, _key) = DaemonConfig::load_or_generate(vault_path, None).await.unwrap();

        // Should have a valid PeerId (64-char hex, non-zero as_u64)
        assert_eq!(config.peer_id.to_string().len(), 64);
        assert_ne!(config.peer_id.as_u64(), 0);
        assert!(config.legacy_peer_id.is_none());

        // Both key file and config file should exist
        assert!(vault_path.join(".sync/daemon.key").exists());
        assert!(vault_path.join(".sync/daemon.toml").exists());
    }

    #[tokio::test]
    async fn test_daemon_config_persists_across_restarts() {
        let temp_dir = TempDir::new().unwrap();
        let vault_path = temp_dir.path();

        let (config1, _) = DaemonConfig::load_or_generate(vault_path, None).await.unwrap();

        // Simulate restart — should load same PeerId from key file
        let (config2, _) = DaemonConfig::load_or_generate(vault_path, None).await.unwrap();

        assert_eq!(config1.peer_id, config2.peer_id);
    }

    #[tokio::test]
    async fn test_daemon_config_identity_key_override() {
        let temp_dir = TempDir::new().unwrap();
        let vault_path = temp_dir.path();

        // First start with default key
        let (config1, _) = DaemonConfig::load_or_generate(vault_path, None).await.unwrap();

        // Generate an alternate key in a separate location
        let alt_dir = TempDir::new().unwrap();
        let alt_key = IdentityKey::load_or_generate(alt_dir.path()).await.unwrap();
        let alt_peer_id = alt_key.peer_id();
        assert_ne!(alt_peer_id, config1.peer_id);

        // Start with alternate key — should update PeerId in config
        let alt_key_path = alt_dir.path().join(".sync/daemon.key");
        let (config2, _) = DaemonConfig::load_or_generate(vault_path, Some(&alt_key_path)).await.unwrap();
        assert_eq!(config2.peer_id, alt_peer_id);

        // After using the alternate key, the config reflects the alternate PeerId.
        // Default restart now uses the default key again (config tracks last used PeerId).
        let (config3, _) = DaemonConfig::load_or_generate(vault_path, None).await.unwrap();
        assert_eq!(config3.peer_id, config1.peer_id);
    }

    #[tokio::test]
    async fn test_daemon_config_migrates_legacy_peer_id() {
        let temp_dir = TempDir::new().unwrap();
        let vault_path = temp_dir.path();

        // Simulate an old install: daemon.toml exists but daemon.key does not
        let sync_dir = vault_path.join(".sync");
        fs::create_dir_all(&sync_dir).unwrap();
        let old_config = r#"peer_id = "a1b2c3d4e5f67890"
incarnation = 3
"#;
        fs::write(sync_dir.join("daemon.toml"), old_config).unwrap();

        // Also create a fake known_peers.json to verify it gets cleared
        fs::write(sync_dir.join("known_peers.json"), r#"{"peers":[]}"#).unwrap();

        // Load with new code — should generate a new ed25519 key and migrate
        let (config, _) = DaemonConfig::load_or_generate(vault_path, None).await.unwrap();

        // New PeerId should be 64-char hex (ed25519)
        assert_eq!(config.peer_id.to_string().len(), 64);

        // Legacy peer_id should be recorded (the raw 16-char string from the old config)
        assert_eq!(config.legacy_peer_id.as_deref(), Some("a1b2c3d4e5f67890"));

        // known_peers.json should be cleared
        assert!(!sync_dir.join("known_peers.json").exists());

        // daemon.key should now exist
        assert!(sync_dir.join("daemon.key").exists());
    }

    #[tokio::test]
    async fn test_daemon_config_relay_url_persists() {
        let temp_dir = TempDir::new().unwrap();
        let vault_path = temp_dir.path();

        let (mut config, _) = DaemonConfig::load_or_generate(vault_path, None).await.unwrap();

        // Write relay URL
        config.set_relay_url(Some("http://127.0.0.1:3340/".into()), vault_path).unwrap();

        // Reload and verify the URL is stored in the file
        let contents = fs::read_to_string(vault_path.join(".sync/daemon.toml")).unwrap();
        assert!(contents.contains("relay_url"), "relay_url should be in daemon.toml");
        assert!(contents.contains("3340"), "relay URL should contain port");

        // Clear the URL
        config.set_relay_url(None, vault_path).unwrap();

        let contents = fs::read_to_string(vault_path.join(".sync/daemon.toml")).unwrap();
        assert!(!contents.contains("relay_url"), "relay_url should be absent after clearing");
    }

    #[tokio::test]
    async fn test_daemon_config_loads_without_relay_url() {
        let temp_dir = TempDir::new().unwrap();
        let vault_path = temp_dir.path();

        // Simulate an older daemon.toml without relay_url
        let sync_dir = vault_path.join(".sync");
        fs::create_dir_all(&sync_dir).unwrap();

        let key = IdentityKey::load_or_generate(vault_path).await.unwrap();
        let peer_id = key.peer_id();
        fs::write(
            sync_dir.join("daemon.toml"),
            format!("peer_id = \"{peer_id}\"\n"),
        ).unwrap();

        // Should load without error — relay_url field is optional
        let (config, _) = DaemonConfig::load_or_generate(vault_path, None).await.unwrap();
        assert!(config.relay_url.is_none());
    }

    #[tokio::test]
    async fn test_daemon_config_ignores_incarnation_field() {
        let temp_dir = TempDir::new().unwrap();
        let vault_path = temp_dir.path();

        // Write an old-style daemon.toml with incarnation field and a pre-existing key
        let sync_dir = vault_path.join(".sync");
        fs::create_dir_all(&sync_dir).unwrap();

        // Generate a key first so this isn't treated as a legacy upgrade
        let key = IdentityKey::load_or_generate(vault_path).await.unwrap();
        let peer_id = key.peer_id();

        let config_path = sync_dir.join("daemon.toml");
        fs::write(
            &config_path,
            format!("peer_id = \"{peer_id}\"\nincarnation = 5\n"),
        )
        .unwrap();

        // Should load without error, ignoring incarnation
        let (config, _) = DaemonConfig::load_or_generate(vault_path, None).await.unwrap();
        assert_eq!(config.peer_id, peer_id);

        // Saved file should no longer have incarnation field
        let contents = fs::read_to_string(&config_path).unwrap();
        assert!(!contents.contains("incarnation"));
    }
}
