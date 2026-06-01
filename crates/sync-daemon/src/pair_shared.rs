//! Shared post-pairing logic used by both the desktop tray and the CLI.
//!
//! When a pairing initiator successfully joins an existing mesh, both paths must
//! perform the same onboarding steps: write the mesh roster into the local
//! allowlist, adopt the mesh's VaultId so the device lands on the right gossip
//! topic, and persist the mesh's relay URL for the next daemon start. These
//! helpers exist so the logic lives in exactly one place rather than being
//! duplicated (and drifting) across the two entry points.

use std::path::Path;

use sync_core::SyncMetadata;
use sync_core::allowlist::{AllowedPeer, AllowlistStorage};
use sync_core::network::SyncNode;
use sync_core::peer_id::{PeerId, VaultId};
use tracing::warn;

use crate::native_fs::NativeFs;
use crate::persistence::DaemonConfig;

/// Write the mesh roster into the local allowlist after a successful pair.
///
/// On the very first pair (empty allowlist) the device adds *itself* so the
/// responder's sync requests are accepted once gossip joins. Every mesh member
/// returned by the pairing exchange is then added under the placeholder name
/// "unknown"; the real device names self-heal once a gossip `AllowlistUpdate`
/// arrives from the mesh. Failures are logged, not fatal — a missing allowlist
/// entry degrades to "this peer can't sync yet", not a broken pairing.
pub async fn write_pair_allowlist<AL: AllowlistStorage>(
    allowlist: &AL,
    self_peer_id: PeerId,
    self_device_name: &str,
    mesh_members: &[PeerId],
) {
    // Bootstrap the allowlist on first pair: add self so the responder side's
    // sync requests are accepted once gossip joins.
    if matches!(allowlist.list_peers().await, Ok(peers) if peers.is_empty()) {
        if let Err(e) = allowlist.add_peer(self_peer_id, self_device_name).await {
            warn!("Failed to add self to allowlist on first pair: {}", e);
        }
    }

    for member_id in mesh_members {
        let peer = AllowedPeer::new(*member_id, "unknown");
        if let Err(e) = allowlist.add_peer(peer.node_id, &peer.device_name).await {
            warn!(
                "Failed to add mesh member {} to allowlist: {}",
                member_id, e
            );
        }
    }
}

/// Rewrite `.sync/metadata.toml` to adopt the mesh's VaultId (CLI path).
///
/// The CLI process does not hold a live in-memory `Vault` during pairing, so it
/// adopts by rewriting the on-disk metadata directly. The next `memory sync up`
/// reads the adopted VaultId and joins the correct gossip topic. The tray path
/// instead uses the live `Vault::adopt_vault_id` so its in-memory id stays in
/// sync; this on-disk variant is the equivalent for the exit-then-restart CLI.
pub async fn adopt_vault_id_on_disk(vault_path: &Path, new_id: VaultId) -> anyhow::Result<()> {
    let fs = NativeFs::new(vault_path.to_path_buf());
    let existing = SyncMetadata::load_or_migrate(&fs).await?;
    if existing.vault_id == new_id {
        return Ok(());
    }
    let meta = SyncMetadata {
        version: existing.version,
        vault_id: new_id,
    };
    meta.save(&fs).await?;
    Ok(())
}

/// Persist the mesh's relay URL to `daemon.toml` for the next daemon start.
///
/// The joiner has no relay URL of its own, so cross-network sync would silently
/// fail without this. We persist the first non-empty entry. This is a
/// persist-for-next-start step, NOT a live endpoint rebuild: the adopted relay
/// takes effect when the daemon next starts and reads `daemon.toml`. For v0.5.x
/// LAN pairing this limitation is invisible — LAN sync uses direct QUIC + mDNS
/// and doesn't need the relay; the persisted URL benefits the next cross-network
/// session.
///
/// Loading `DaemonConfig::load_or_generate` on a fresh vault generates the device
/// keypair as a side effect; that is acceptable — a device pairing in needs an
/// identity anyway.
pub async fn persist_adopted_relay(vault_path: &Path, relay_urls: &[String]) {
    let Some(url) = relay_urls.iter().find(|u| !u.is_empty()).cloned() else {
        return;
    };

    match DaemonConfig::load_or_generate(vault_path, None).await {
        Ok((mut config, _identity)) => {
            if let Err(e) = config.set_relay_url(Some(url), vault_path) {
                warn!("Failed to persist adopted relay URL: {}", e);
            }
        }
        Err(e) => {
            warn!(
                "Failed to load daemon config to persist adopted relay URL: {}",
                e
            );
        }
    }
}

/// Recover the mesh's VaultId from a `PairingResult`'s gossip topic bytes.
///
/// The VaultId travels over the wire only inside the topic, so this is the
/// single defined way to recover it on the initiator side. Returns `None` when
/// the topic is absent (only on a failed pair, which both callers handle first).
pub fn vault_id_from_pairing_topic(vault_topic: Option<[u8; 32]>) -> Option<VaultId> {
    vault_topic.as_ref().map(SyncNode::vault_id_from_topic)
}
