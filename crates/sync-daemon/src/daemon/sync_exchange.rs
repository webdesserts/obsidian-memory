//! Background QUIC sync-exchange helper shared by `on_neighbor_up` and
//! `on_change_received`.
//!
//! Both handlers follow the same tail: connect to a peer, send a SyncRequest,
//! process the response, and update last-seen timestamps. Only the log messages
//! and a `path` argument (present only for change-pull) differ.

use std::sync::Arc;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use tracing::{debug, error, info, warn};

use iroh::EndpointId;
use sync_core::allowlist::AllowlistStorage;
use sync_core::fs::FileSystem;
use sync_core::{PeerId, PeerRegistry, Vault};

use super::{ExchangeLearned, now_ms};

/// Extract the peer's active relay URL from an iroh `remote_info` snapshot.
///
/// Returns the first address that is both currently `Active` and a relay path —
/// that is the URL the peer is actually reachable at right now, which is what
/// learn-on-exchange should refresh the stored hint to. A LAN-direct connection
/// has no active relay path, yielding `None` (we still stamp success, we just
/// don't overwrite the stored URL with nothing).
fn active_relay_url(info: &iroh::endpoint::RemoteInfo) -> Option<iroh::RelayUrl> {
    use iroh::TransportAddr;
    use iroh::endpoint::TransportAddrUsage;

    info.addrs().find_map(|a| match (a.usage(), a.addr()) {
        (TransportAddrUsage::Active, TransportAddr::Relay(url)) => Some(url.clone()),
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
    target: EndpointId,
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
    tokio::spawn(async move {
        let response_bytes = match sync_core::network::streams::connect_and_sync_raw(
            &endpoint,
            target,
            &request_bytes,
        )
        .await
        {
            Ok(bytes) => bytes,
            Err(e) => {
                match &kind {
                    SyncExchangeKind::NeighborUp => {
                        // NeighborUp fires before the peer's QUIC listener is ready on some
                        // OS configurations. Log a warning and move on — the peer will
                        // initiate a sync in the other direction.
                        warn!("Failed to connect for sync with {}: {}", target, e);
                    }
                    SyncExchangeKind::ChangePull { .. } => {
                        warn!("Failed to pull change from {}: {}", target, e);
                    }
                }
                return;
            }
        };

        // Process the response under the vault lock, capturing only the result
        // we need afterward so the lock drops before the relay-learn query.
        let process_result = {
            let vault = vault.lock().await;
            vault.process_sync_message(&response_bytes).await
        };

        let modified_paths = match process_result {
            Ok((_, modified_paths)) => modified_paths,
            Err(e) => {
                match &kind {
                    SyncExchangeKind::NeighborUp => {
                        error!("Failed to process sync response from {}: {}", target, e);
                    }
                    SyncExchangeKind::ChangePull { .. } => {
                        error!("Failed to process pulled change from {}: {}", target, e);
                    }
                }
                return;
            }
        };

        peer_registry.lock().await.update_last_seen(&peer_id);
        if let Err(e) = allowlist.update_last_seen(&peer_id, now_ms()).await {
            warn!(
                "Failed to update allowlist last_seen for {}: {}",
                target, e
            );
        }

        match &kind {
            SyncExchangeKind::NeighborUp => {
                if !modified_paths.is_empty() {
                    info!(
                        "Synced {} file(s) from {} on NeighborUp",
                        modified_paths.len(),
                        target
                    );
                } else {
                    debug!("Full sync with {} complete — no changes", target);
                }
            }
            SyncExchangeKind::ChangePull { path } => {
                if !modified_paths.is_empty() {
                    info!(
                        "Pulled {} file(s) from {} after change notification on {}",
                        modified_paths.len(),
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
            .remote_info(target)
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
