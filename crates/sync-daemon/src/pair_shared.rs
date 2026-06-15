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
///
/// `self_relay_url` is this device's own advertised relay URL, if it is running
/// one. It is threaded through to the persist helper's clobber-guard so that
/// adopting the responder's relay does NOT drop our own advertised `relay_url`
/// from `daemon.toml`. The CLI pairing flow has no running relay and passes
/// `None`; the live-daemon initiator flow passes its advertised URL.
pub async fn persist_adopted_relay(
    vault_path: &Path,
    responder_id: EndpointId,
    relay_urls: &[String],
    self_relay_url: Option<String>,
    sync_node: &SyncNode,
) {
    let Some(url_str) = relay_urls.iter().find(|u| !u.is_empty()).cloned() else {
        return;
    };

    let responder_id_hex = responder_id.to_string();
    let now = crate::daemon::now_ms();

    if let Err(e) = crate::persistence::persist_config_change(vault_path, self_relay_url, |config| {
        config.upsert_peer_relay(&responder_id_hex, &url_str, now)
    })
    .await
    {
        warn!("Failed to persist adopted relay URL: {}", e);
        return;
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

    /// Regression: adopting a pairing relay must NOT clobber the daemon's own
    /// advertised `relay_url` out of `daemon.toml`.
    ///
    /// `persist_adopted_relay` does a load → mutate → save round-trip, and
    /// `load_or_generate` deliberately drops `relay_url` (runtime state). Before
    /// the fix, the function never re-stamped it, so an active daemon (running
    /// its own relay) that paired in would silently lose its advertised URL.
    /// Threading `self_relay_url` through to the persist helper's clobber-guard
    /// is the fix; this proves the URL survives the adopt-write.
    #[tokio::test]
    async fn persist_adopted_relay_preserves_own_relay_url() {
        use crate::persistence::DaemonConfig;
        use iroh::SecretKey;
        use std::sync::Arc;
        use tempfile::TempDir;

        let vault_dir = TempDir::new().unwrap();
        let vault_path = vault_dir.path();

        // Simulate our daemon having started its own relay: write our advertised
        // URL to daemon.toml (this is what set_relay_url does on relay start).
        let own_relay_url = "http://my-own-relay:3340/".to_string();
        let (mut config, identity) = DaemonConfig::load_or_generate(vault_path, None)
            .await
            .unwrap();
        config
            .set_relay_url(Some(own_relay_url.clone()), vault_path)
            .unwrap();

        // Build a minimal SyncNode (no relay) just so the live-lookup seed step
        // of persist_adopted_relay has something to call. Use our persisted
        // identity so node_id() matches the config's peer_id.
        let allowlist = Arc::new(InMemoryAllowlist::new());
        let sync_node = SyncNode::new(identity.secret_key_bytes(), None, allowlist)
            .await
            .expect("failed to build SyncNode for test");

        // The responder is a DIFFERENT device, so its EndpointId won't trip the
        // self-skip in upsert_peer_relay.
        let responder_secret = SecretKey::from_bytes(&[7u8; 32]);
        let responder_id = responder_secret.public();
        let responder_relay = "http://responder-relay:3340/".to_string();

        persist_adopted_relay(
            vault_path,
            responder_id,
            &[responder_relay.clone()],
            Some(own_relay_url.clone()),
            &sync_node,
        )
        .await;

        let contents = std::fs::read_to_string(vault_path.join(".sync/daemon.toml")).unwrap();

        // The fix: our own advertised relay_url must survive the adopt-write.
        assert!(
            contents.contains(&format!("relay_url = \"{own_relay_url}\"")),
            "the daemon's own relay_url must NOT be clobbered when adopting a \
             pairing relay; daemon.toml was:\n{contents}"
        );

        // And the responder's relay was actually persisted as a peer hint.
        let (reloaded, _) = DaemonConfig::load_or_generate(vault_path, None)
            .await
            .unwrap();
        let hint = reloaded
            .peer_relays
            .iter()
            .find(|r| r.endpoint_id == responder_id.to_string())
            .expect("responder's relay should be persisted to peer_relays");
        assert_eq!(hint.relay_url, responder_relay);

        sync_node.shutdown().await.ok();
    }
}
