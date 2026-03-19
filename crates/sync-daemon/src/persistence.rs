//! Persistence for daemon state.
//!
//! - **`IdentityKey`** (`.sync/daemon.key`): Ed25519 secret key for the daemon's network identity
//! - **`DaemonConfig`** (`.sync/daemon.toml`): Daemon config including the derived PeerId

use anyhow::{Context, Result};
use ed25519_dalek::SigningKey;
use serde::{Deserialize, Serialize};
use std::fs;
use std::path::Path;
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
    /// Human-readable name for this mesh, broadcast via mDNS.
    ///
    /// Defaults to `None`, which causes the daemon to fall back to the vault
    /// directory name when advertising. Set this to override the displayed name
    /// on peer devices during discovery and pairing.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub mesh_name: Option<String>,
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
                mesh_name: existing.mesh_name,
            };
            info!(peer_id = %config.peer_id, "Loaded daemon config");
            config
        } else {
            let config = DaemonConfig {
                peer_id: new_peer_id,
                legacy_peer_id: None,
                relay_url: None,
                mesh_name: None,
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
/// `legacy_peer_id`, `relay_url`, and `mesh_name` fields from older `daemon.toml`
/// files without failing.
#[derive(Deserialize)]
struct DaemonConfigRaw {
    peer_id: PeerId,
    legacy_peer_id: Option<String>,
    /// Parsed from disk but intentionally discarded on load — relay URL is runtime state
    /// that gets re-written after the relay starts. A stale URL from a crashed run is meaningless.
    #[allow(dead_code)]
    relay_url: Option<String>,
    /// Parsed from disk but discarded — incarnation was removed with the SWIM protocol.
    #[allow(dead_code)]
    incarnation: Option<u64>,
    mesh_name: Option<String>,
}

/// Clear `known_peers.json` after a PeerId migration.
///
/// Peer entries are keyed by old PeerIds that no longer match after migration.
/// Clearing is simpler than re-keying.
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


#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

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
