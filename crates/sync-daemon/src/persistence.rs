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
        let bytes: [u8; 32] = bytes.try_into().map_err(|_| {
            anyhow::anyhow!("Key file must be exactly 32 bytes: {}", key_path.display())
        })?;
        Ok(Self::from_bytes(bytes))
    }

    /// Build an `IdentityKey` from raw secret key bytes.
    fn from_bytes(bytes: [u8; 32]) -> Self {
        Self {
            signing_key: SigningKey::from_bytes(&bytes),
        }
    }

    /// Derive the `PeerId` from this identity key's public key.
    ///
    /// Delegates to `PeerId::from_secret_bytes` so the secret-key → device
    /// PeerId derivation lives in one place, shared with the WASM plugin.
    pub fn peer_id(&self) -> PeerId {
        PeerId::from_secret_bytes(self.signing_key.to_bytes())
    }

    /// Return the raw 32-byte ed25519 secret key.
    ///
    /// Used to initialize the iroh `SyncNode`, which needs the secret key to
    /// derive a stable `EndpointId` (the iroh equivalent of our `PeerId`).
    pub fn secret_key_bytes(&self) -> [u8; 32] {
        self.signing_key.to_bytes()
    }
}

/// A learned relay URL for a specific peer, persisted so the daemon can seed
/// its address-lookup service on startup without waiting for re-pairing.
///
/// Relay hints are keyed by the peer's `EndpointId` (64-char hex). The relay
/// URL is stored as-is and parsed at seed time so invalid entries can be
/// skipped gracefully rather than failing config load.
///
/// Unlike `relay_url` (this node's own relay — runtime state, discarded on
/// load), peer relay entries survive restarts. They are updated on re-pairing
/// with last-write-wins semantics.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct PeerRelay {
    /// The peer's iroh `EndpointId` as a 64-char lowercase hex string.
    pub endpoint_id: String,
    /// The peer's advertised relay URL (e.g. `http://umbra.computer:3340/`).
    pub relay_url: String,
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
    /// Learned relay URLs for known peers.
    ///
    /// Used to seed the address-lookup service at startup so gossip can reach
    /// off-LAN peers through their relay before any re-pairing occurs. Updated
    /// on pairing with last-write-wins semantics per `endpoint_id`.
    ///
    /// Unlike `relay_url` (this node's own relay — runtime/discarded), these
    /// entries survive restarts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub peer_relays: Vec<PeerRelay>,
}

impl DaemonConfig {
    /// Load or generate daemon config, deriving PeerId from the identity key.
    ///
    /// Returns `(config, identity_key)` so the caller can access the raw secret
    /// key bytes needed to initialize the iroh `SyncNode`.
    ///
    /// `identity_key_path` — if provided, loads a custom key file instead of the
    /// default `.sync/daemon.key`. This replaces the old `--peer-id` flag.
    pub async fn load_or_generate(
        vault_path: &Path,
        identity_key_path: Option<&Path>,
    ) -> Result<(Self, IdentityKey)> {
        let config_path = vault_path.join(DAEMON_CONFIG_FILE);
        let key_path = vault_path.join(DAEMON_KEY_FILE);

        // Detect legacy upgrade: daemon.toml exists but daemon.key doesn't.
        // This means the daemon was previously running with the old u64 PeerId format.
        let is_legacy_upgrade =
            config_path.exists() && !key_path.exists() && identity_key_path.is_none();

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
                // Peer relay hints are durable — preserved across restarts so the
                // daemon can seed address-lookup without re-pairing.
                peer_relays: existing.peer_relays,
            };
            info!(peer_id = %config.peer_id, "Loaded daemon config");
            config
        } else {
            let config = DaemonConfig {
                peer_id: new_peer_id,
                legacy_peer_id: None,
                relay_url: None,
                mesh_name: None,
                peer_relays: vec![],
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

    /// Record a peer's relay URL keyed by their `EndpointId`, then persist.
    ///
    /// This is the write path for learned relay hints — called on pairing so
    /// the daemon can reach this peer through their relay on future startups,
    /// even when mDNS isn't available (off-LAN).
    ///
    /// **Dedup:** if an entry for `endpoint_id` already exists it is overwritten
    /// (last-write-wins), so re-pairing naturally updates a stale relay URL.
    ///
    /// **Self-skip:** if `endpoint_id` matches `self.peer_id` (our own identity)
    /// the call is a no-op — seeding ourselves into the lookup would cause iroh
    /// to try to dial itself.
    pub fn upsert_peer_relay(
        &mut self,
        endpoint_id: &str,
        relay_url: &str,
        vault_path: &Path,
    ) -> Result<()> {
        // Skip self: our peer_id hex and the EndpointId hex share the same
        // underlying ed25519 public key bytes.
        if self.peer_id.to_string() == endpoint_id {
            return Ok(());
        }

        // Replace in-place if endpoint_id already present, else append.
        if let Some(existing) = self
            .peer_relays
            .iter_mut()
            .find(|r| r.endpoint_id == endpoint_id)
        {
            existing.relay_url = relay_url.to_string();
        } else {
            self.peer_relays.push(PeerRelay {
                endpoint_id: endpoint_id.to_string(),
                relay_url: relay_url.to_string(),
            });
        }

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
/// `legacy_peer_id`, `relay_url`, `mesh_name`, and `peer_relays` fields from
/// older `daemon.toml` files without failing.
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
    /// Preserved on load — peer relay hints survive restarts, unlike our own relay_url.
    #[serde(default)]
    peer_relays: Vec<PeerRelay>,
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

        let (config, _key) = DaemonConfig::load_or_generate(vault_path, None)
            .await
            .unwrap();

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

        let (config1, _) = DaemonConfig::load_or_generate(vault_path, None)
            .await
            .unwrap();

        // Simulate restart — should load same PeerId from key file
        let (config2, _) = DaemonConfig::load_or_generate(vault_path, None)
            .await
            .unwrap();

        assert_eq!(config1.peer_id, config2.peer_id);
    }

    #[tokio::test]
    async fn test_daemon_config_identity_key_override() {
        let temp_dir = TempDir::new().unwrap();
        let vault_path = temp_dir.path();

        // First start with default key
        let (config1, _) = DaemonConfig::load_or_generate(vault_path, None)
            .await
            .unwrap();

        // Generate an alternate key in a separate location
        let alt_dir = TempDir::new().unwrap();
        let alt_key = IdentityKey::load_or_generate(alt_dir.path()).await.unwrap();
        let alt_peer_id = alt_key.peer_id();
        assert_ne!(alt_peer_id, config1.peer_id);

        // Start with alternate key — should update PeerId in config
        let alt_key_path = alt_dir.path().join(".sync/daemon.key");
        let (config2, _) = DaemonConfig::load_or_generate(vault_path, Some(&alt_key_path))
            .await
            .unwrap();
        assert_eq!(config2.peer_id, alt_peer_id);

        // After using the alternate key, the config reflects the alternate PeerId.
        // Default restart now uses the default key again (config tracks last used PeerId).
        let (config3, _) = DaemonConfig::load_or_generate(vault_path, None)
            .await
            .unwrap();
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
        let (config, _) = DaemonConfig::load_or_generate(vault_path, None)
            .await
            .unwrap();

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

        let (mut config, _) = DaemonConfig::load_or_generate(vault_path, None)
            .await
            .unwrap();

        // Write relay URL
        config
            .set_relay_url(Some("http://127.0.0.1:3340/".into()), vault_path)
            .unwrap();

        // Reload and verify the URL is stored in the file
        let contents = fs::read_to_string(vault_path.join(".sync/daemon.toml")).unwrap();
        assert!(
            contents.contains("relay_url"),
            "relay_url should be in daemon.toml"
        );
        assert!(contents.contains("3340"), "relay URL should contain port");

        // Clear the URL
        config.set_relay_url(None, vault_path).unwrap();

        let contents = fs::read_to_string(vault_path.join(".sync/daemon.toml")).unwrap();
        assert!(
            !contents.contains("relay_url"),
            "relay_url should be absent after clearing"
        );
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
        )
        .unwrap();

        // Should load without error — relay_url field is optional
        let (config, _) = DaemonConfig::load_or_generate(vault_path, None)
            .await
            .unwrap();
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
        let (config, _) = DaemonConfig::load_or_generate(vault_path, None)
            .await
            .unwrap();
        assert_eq!(config.peer_id, peer_id);

        // Saved file should no longer have incarnation field
        let contents = fs::read_to_string(&config_path).unwrap();
        assert!(!contents.contains("incarnation"));
    }

    // ==================== peer_relays tests ====================

    /// `peer_relays` entries round-trip through save/load.
    #[tokio::test]
    async fn test_peer_relays_round_trip() {
        let temp_dir = TempDir::new().unwrap();
        let vault_path = temp_dir.path();

        let (mut config, _) = DaemonConfig::load_or_generate(vault_path, None)
            .await
            .unwrap();

        // Seed a peer relay entry using a plausible endpoint_id (64 hex chars, different
        // from our own peer_id so self-skip doesn't apply).
        let peer_id_hex = "a".repeat(64);
        config
            .upsert_peer_relay(&peer_id_hex, "http://umbra.computer:3340/", vault_path)
            .unwrap();

        // Reload and verify the entry survives.
        let (loaded, _) = DaemonConfig::load_or_generate(vault_path, None)
            .await
            .unwrap();

        assert_eq!(loaded.peer_relays.len(), 1);
        assert_eq!(loaded.peer_relays[0].endpoint_id, peer_id_hex);
        assert_eq!(
            loaded.peer_relays[0].relay_url,
            "http://umbra.computer:3340/"
        );
    }

    /// `upsert_peer_relay` with the same endpoint_id overwrites the URL (last-write-wins),
    /// not appending a duplicate.
    #[tokio::test]
    async fn test_peer_relays_upsert_dedup() {
        let temp_dir = TempDir::new().unwrap();
        let vault_path = temp_dir.path();

        let (mut config, _) = DaemonConfig::load_or_generate(vault_path, None)
            .await
            .unwrap();

        let peer_id_hex = "b".repeat(64);

        config
            .upsert_peer_relay(&peer_id_hex, "http://old-relay:3340/", vault_path)
            .unwrap();
        config
            .upsert_peer_relay(&peer_id_hex, "http://new-relay:3340/", vault_path)
            .unwrap();

        // Should have exactly one entry with the updated URL.
        assert_eq!(config.peer_relays.len(), 1);
        assert_eq!(config.peer_relays[0].relay_url, "http://new-relay:3340/");

        // Reload to confirm it persisted correctly.
        let (loaded, _) = DaemonConfig::load_or_generate(vault_path, None)
            .await
            .unwrap();
        assert_eq!(loaded.peer_relays.len(), 1);
        assert_eq!(loaded.peer_relays[0].relay_url, "http://new-relay:3340/");
    }

    /// An old daemon.toml without `peer_relays` loads fine (migration — field defaults to empty).
    #[tokio::test]
    async fn test_peer_relays_migration_from_old_config() {
        let temp_dir = TempDir::new().unwrap();
        let vault_path = temp_dir.path();

        let sync_dir = vault_path.join(".sync");
        fs::create_dir_all(&sync_dir).unwrap();

        // Write a daemon.toml that predates the peer_relays field.
        let key = IdentityKey::load_or_generate(vault_path).await.unwrap();
        let peer_id = key.peer_id();
        fs::write(
            sync_dir.join("daemon.toml"),
            format!("peer_id = \"{peer_id}\"\n"),
        )
        .unwrap();

        // Should load without error; peer_relays defaults to empty.
        let (config, _) = DaemonConfig::load_or_generate(vault_path, None)
            .await
            .unwrap();
        assert!(
            config.peer_relays.is_empty(),
            "peer_relays should be empty when the field is absent from the file"
        );
    }

    /// `upsert_peer_relay` skips entries whose endpoint_id matches our own peer_id —
    /// seeding ourselves into the lookup would cause iroh to try to dial itself.
    #[tokio::test]
    async fn test_peer_relays_skips_self() {
        let temp_dir = TempDir::new().unwrap();
        let vault_path = temp_dir.path();

        let (mut config, _) = DaemonConfig::load_or_generate(vault_path, None)
            .await
            .unwrap();

        // Use our own peer_id as the endpoint_id — should be silently skipped.
        let own_id = config.peer_id.to_string();
        config
            .upsert_peer_relay(&own_id, "http://self-relay:3340/", vault_path)
            .unwrap();

        assert!(
            config.peer_relays.is_empty(),
            "should not add self to peer_relays"
        );
    }

    /// `relay_url` (this node's own relay) is still discarded on load —
    /// the new `peer_relays` field must not regress that behavior.
    #[tokio::test]
    async fn test_relay_url_still_discarded_on_load() {
        let temp_dir = TempDir::new().unwrap();
        let vault_path = temp_dir.path();

        let (mut config, _) = DaemonConfig::load_or_generate(vault_path, None)
            .await
            .unwrap();

        // Simulate a relay starting and writing its URL to daemon.toml.
        config
            .set_relay_url(Some("http://127.0.0.1:3340/".into()), vault_path)
            .unwrap();

        // The file should contain the relay_url.
        let contents = fs::read_to_string(vault_path.join(".sync/daemon.toml")).unwrap();
        assert!(contents.contains("relay_url"), "relay_url should be written");

        // Reload — relay_url must come back as None (runtime state, not preserved).
        let (reloaded, _) = DaemonConfig::load_or_generate(vault_path, None)
            .await
            .unwrap();
        assert!(
            reloaded.relay_url.is_none(),
            "relay_url must be discarded on load (runtime state)"
        );
    }
}
