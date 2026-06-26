//! Background QUIC sync-exchange helper shared by `on_neighbor_up` and
//! `on_change_received`.
//!
//! Both handlers follow the same tail: open a bi-stream to a peer, drive the
//! variable-length vault-sync handshake to termination (via the outbound pump in
//! `sync_stream`), and update last-seen timestamps. Only the log messages and a
//! `path` argument (present only for change-pull) differ.

use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use iroh::EndpointId;
use p2p_core::RelayAddr;
use sync_core::allowlist::AllowlistStorage;
use sync_core::{PeerId, PeerRegistry};
use vault_sync::Vault;
use vault_sync::fs::FileSystem;

use super::{ExchangeLearned, now_ms};

/// Extract the peer's active relay URL from an iroh `remote_info` snapshot.
///
/// Returns the first address that is both currently `Active` and a relay path —
/// that is the URL the peer is actually reachable at right now, which is what
/// learn-on-exchange should refresh the stored hint to. A LAN-direct connection
/// has no active relay path, yielding `None` (we still stamp success, we just
/// don't overwrite the stored URL with nothing).
fn active_relay_url(info: &iroh::endpoint::RemoteInfo) -> Option<RelayAddr> {
    use iroh::TransportAddr;
    use iroh::endpoint::TransportAddrUsage;

    info.addrs().find_map(|a| match (a.usage(), a.addr()) {
        // The relay comes off a raw iroh snapshot; round-trip it through its
        // string form (lossless for a valid relay URL) so the daemon hands the
        // run loop a `RelayAddr` and never names iroh's URL type.
        (TransportAddrUsage::Active, TransportAddr::Relay(url)) => {
            RelayAddr::parse(&url.to_string()).ok()
        }
        _ => None,
    })
}

/// Identifies which event triggered the sync exchange, driving log output.
pub(super) enum SyncExchangeKind {
    /// Triggered by a gossip `NeighborUp` event — full bidirectional sync.
    ///
    /// Logs "Synced N file(s) from X on NeighborUp" on success, and a `debug!`
    /// when no changes were applied.
    NeighborUp,
    /// Triggered by a gossip change notification — pull the changed file.
    ///
    /// Includes the path that triggered the notification for log context.
    /// Does NOT log when there are no changes (the `else` branch is omitted —
    /// this matches the original `on_change_received` behavior).
    ChangePull {
        /// The file path from the gossip change notification.
        path: String,
    },
}

/// Spawn a background task that connects to `target`, sends `request_bytes`,
/// processes the response, and updates last-seen timestamps.
///
/// The `kind` parameter controls which log messages are emitted. The caller
/// is responsible for the allowlist check and sync-request preparation before
/// calling this — the spawned task inherits pre-cloned Arc handles and does not
/// need to re-acquire `&mut self`.
#[allow(clippy::too_many_arguments)]
pub(super) fn spawn_sync_exchange<FS, AL>(
    target: PeerId,
    peer_id: PeerId,
    request_bytes: Vec<u8>,
    vault: Arc<Mutex<Vault<FS>>>,
    allowlist: Arc<AL>,
    peer_registry: Arc<Mutex<PeerRegistry>>,
    endpoint: iroh::Endpoint,
    exchange_learn_tx: mpsc::UnboundedSender<ExchangeLearned>,
    kind: SyncExchangeKind,
) where
    FS: FileSystem + 'static,
    AL: AllowlistStorage + 'static,
{
    // The raw QUIC connect + remote_info lookup need iroh's id. `target` is
    // transport-sourced (this exchange is driven by a live gossip neighbour /
    // received change), so the conversion is infallible for it.
    let target_endpoint = EndpointId::from_bytes(target.as_bytes())
        .expect("sync-exchange target is a live, transport-sourced peer id");
    tokio::spawn(async move {
        // Drive the variable-length vault-sync handshake to termination over one
        // bi-stream (digest opener → … → terminal `reply: None`), processing each
        // leg on the local vault. The pump scopes the vault lock to each
        // `process_message` (never across a wire await) so a concurrent
        // reverse-initiated sync can't deadlock against it.
        let modified = match super::sync_stream::pump_outbound(
            &endpoint,
            target_endpoint,
            &request_bytes,
            &vault,
        )
        .await
        {
            Ok(modified) => modified,
            Err(e) => {
                match &kind {
                    SyncExchangeKind::NeighborUp => {
                        // NeighborUp fires before the peer's QUIC listener is ready on some
                        // OS configurations. Log a warning and move on — the peer will
                        // initiate a sync in the other direction.
                        warn!("Failed to sync with {}: {}", target, e);
                    }
                    SyncExchangeKind::ChangePull { .. } => {
                        warn!("Failed to pull change from {}: {}", target, e);
                    }
                }
                return;
            }
        };

        peer_registry.lock().await.update_last_seen(&peer_id);
        if let Err(e) = allowlist.update_last_seen(&peer_id, now_ms()).await {
            warn!("Failed to update allowlist last_seen for {}: {}", target, e);
        }

        match &kind {
            SyncExchangeKind::NeighborUp => {
                if !modified.is_empty() {
                    info!(
                        "Synced {} file(s) from {} on NeighborUp",
                        modified.len(),
                        target
                    );
                } else {
                    debug!("Full sync with {} complete — no changes", target);
                }
            }
            SyncExchangeKind::ChangePull { path } => {
                if !modified.is_empty() {
                    info!(
                        "Pulled {} file(s) from {} after change notification on {}",
                        modified.len(),
                        target,
                        path
                    );
                }
                // No `else` branch: `on_change_received` did not log when
                // no changes were applied. Preserved intentionally.
            }
        }

        // Learn-on-exchange: the peer is reachable, so refresh its hint. The
        // vault lock is already dropped (above), so this actor round-trip never
        // extends the lock-hold. A LAN-direct connection yields no active relay
        // path → `None`, which still resets freshness without inventing a URL.
        let relay_url = endpoint
            .remote_info(target_endpoint)
            .await
            .as_ref()
            .and_then(active_relay_url);
        // send() only fails if the run-loop receiver is gone (shutdown) — benign.
        let _ = exchange_learn_tx.send(ExchangeLearned {
            endpoint_id: target,
            relay_url,
        });
    });
}
