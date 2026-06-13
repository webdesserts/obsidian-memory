//! Shared post-pairing logic used by both the desktop tray and the CLI.
//!
//! When a pairing initiator successfully joins an existing mesh, both paths must
//! perform the same onboarding steps: write the mesh roster into the local
//! allowlist, adopt the mesh's VaultId so the device lands on the right gossip
//! topic, and persist the mesh's relay URL for the next daemon start. These
//! helpers exist so the logic lives in exactly one place rather than being
//! duplicated (and drifting) across the two entry points.

use std::path::Path;

use iroh::{EndpointId, RelayUrl};
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

/// Persist the responder's relay URL keyed by their `EndpointId` and seed the
/// live address-lookup so the current session can route through their relay
/// without a restart.
///
/// `responder_id` is the transport-verified identity of the QUIC connection the
/// initiator dialed — not inferred from `PairingResult.mesh_members` — so the
/// binding is correct even if `mesh_members` ordering or content differs.
///
/// `relay_urls` is currently always single-source (the responder's own relay),
/// so binding `relay_urls.first()` to `responder_id` is sound. Mesh-wide relay
/// propagation (other members' relays) is deferred.
///
/// **Asymmetry note:** the responder side does NOT learn the initiator's relay
/// today — the `PairingHello` message does not carry a relay URL. That
/// propagation is deferred to a future phase.
///
/// Loading `DaemonConfig::load_or_generate` on a fresh vault generates the
/// device keypair as a side effect; that is acceptable — a device pairing in
/// needs an identity anyway.
pub async fn persist_adopted_relay(
    vault_path: &Path,
    responder_id: EndpointId,
    relay_urls: &[String],
    sync_node: &SyncNode,
) {
    let Some(url_str) = relay_urls.iter().find(|u| !u.is_empty()).cloned() else {
        return;
    };

    let responder_id_hex = responder_id.to_string();

    match DaemonConfig::load_or_generate(vault_path, None).await {
        Ok((mut config, _identity)) => {
            let now = crate::daemon::now_ms();
            if let Err(e) = config.upsert_peer_relay(&responder_id_hex, &url_str, now, vault_path) {
                warn!("Failed to persist adopted relay URL: {}", e);
                return;
            }
        }
        Err(e) => {
            warn!(
                "Failed to load daemon config to persist adopted relay URL: {}",
                e
            );
            return;
        }
    }

    // Seed the live lookup so gossip can route to the responder through their
    // relay in the current session without waiting for a restart.
    if let Ok(relay_url) = url_str.parse::<RelayUrl>() {
        sync_node.add_peer_relay(responder_id, &relay_url);
    } else {
        warn!(
            url = %url_str,
            "Adopted relay URL is not a valid RelayUrl — skipping live lookup seed"
        );
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

#[cfg(test)]
mod tests {
    use super::*;
    use sync_core::allowlist::InMemoryAllowlist;

    fn peer(byte: u8) -> PeerId {
        PeerId::from_secret_bytes([byte; 32])
    }

    /// First pair into an empty allowlist bootstraps self so the responder's
    /// sync requests are accepted, then adds the mesh member.
    #[tokio::test]
    async fn first_pair_adds_self_and_mesh_member() {
        let allowlist = InMemoryAllowlist::new();
        let self_id = peer(1);
        let member = peer(2);

        write_pair_allowlist(&allowlist, self_id, "this-device", &[member]).await;

        let peers = allowlist.list_peers().await.unwrap();
        assert_eq!(peers.len(), 2, "self + one mesh member");

        let self_entry = peers
            .iter()
            .find(|p| p.node_id == self_id)
            .expect("self should be in the allowlist after first pair");
        assert_eq!(self_entry.device_name, "this-device");

        assert!(
            peers.iter().any(|p| p.node_id == member),
            "mesh member should be in the allowlist"
        );
    }

    /// When the allowlist already has entries, pairing does NOT re-add self —
    /// the self-bootstrap is a first-pair-only step — but still adds new members.
    #[tokio::test]
    async fn re_pair_with_nonempty_allowlist_skips_self_bootstrap() {
        let allowlist = InMemoryAllowlist::new();
        let self_id = peer(1);
        let existing_member = peer(2);
        let new_member = peer(3);

        // Pre-seed a member so the allowlist is non-empty going in.
        allowlist
            .add_peer(existing_member, "existing")
            .await
            .unwrap();

        write_pair_allowlist(&allowlist, self_id, "this-device", &[new_member]).await;

        let peers = allowlist.list_peers().await.unwrap();
        assert!(
            !peers.iter().any(|p| p.node_id == self_id),
            "self is only bootstrapped on the first (empty) pair, not re-pairs"
        );
        assert!(peers.iter().any(|p| p.node_id == existing_member));
        assert!(peers.iter().any(|p| p.node_id == new_member));
        assert_eq!(peers.len(), 2, "existing member + new member, no self");
    }

    /// Re-running the same pair is idempotent: members are keyed by node_id, so
    /// a repeated write updates names in place rather than duplicating entries.
    #[tokio::test]
    async fn re_pair_is_idempotent() {
        let allowlist = InMemoryAllowlist::new();
        let self_id = peer(1);
        let member = peer(2);

        write_pair_allowlist(&allowlist, self_id, "this-device", &[member]).await;
        write_pair_allowlist(&allowlist, self_id, "this-device", &[member]).await;

        let peers = allowlist.list_peers().await.unwrap();
        assert_eq!(
            peers.len(),
            2,
            "re-running the pair must not duplicate self or the mesh member"
        );
    }
}
