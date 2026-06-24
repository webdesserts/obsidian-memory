//! Shared test helpers for the sync-daemon integration suites.
//!
//! `relay_integration.rs` and `daemon_integration.rs` both `mod common`. Keep
//! anything here genuinely cross-suite — suite-private helpers live in their own
//! file.

use std::sync::Arc;
use std::time::Duration;

use iroh::RelayUrl;
use sync_core::allowlist::InMemoryAllowlist;
use sync_core::network::{SyncNode, SyncNodeSeam};
use sync_core::peer_id::PeerId;
use tokio::sync::Mutex;
use tokio::sync::mpsc;
use vault_sync::Vault;
use vault_sync::fs::InMemoryFs;

/// Generate a deterministic 32-byte key seed from a small integer.
///
/// Used by relay_integration and daemon_integration tests to build iroh nodes
/// with repeatable identities. Each unique `n` produces a distinct key.
pub fn seed(n: u8) -> [u8; 32] {
    [n; 32]
}

/// A relay-aware test node: an iroh `SyncNode` whose home relay is `relay_url`,
/// plus a vault and allowlist, ready to be handed to `Daemon::new`.
///
/// Unlike `daemon_integration.rs`'s `build_node` (which wires direct
/// `MemoryLookup` addresses), this node's only routing path is its home relay —
/// the topology a non-home `ActiveRelayActor` reap repro needs. Peer-relay hints
/// are seeded afterward via `sync_node.set_peer_relay`, which targets the
/// `peer_lookup` `SyncNode::new` already registered.
///
/// `dead_code` allow: Rust compiles `mod common` separately into each
/// integration-test binary, so this is "unused" from `daemon_integration.rs`'s
/// perspective even though `relay_integration.rs` uses it.
#[allow(dead_code)]
pub struct RelayNode {
    pub vault: Arc<Mutex<Vault<Arc<InMemoryFs>>>>,
    pub fs: Arc<InMemoryFs>,
    pub allowlist: Arc<InMemoryAllowlist>,
    pub sync_node: SyncNode,
    pub node_id: PeerId,
    /// The inbound-sync freshness receiver paired with the node's pumped handler.
    /// Wire into the daemon via `set_inbound_seen_rx` so inbound-only peers count
    /// as alive (S2).
    pub inbound_seen_rx: mpsc::UnboundedReceiver<PeerId>,
}

/// Build a [`RelayNode`] whose home relay is `relay_url`.
///
/// Built with the daemon's PUMPED inbound handler (the relay-path convergence
/// tests run a full `Daemon`, so the responder must drive the multi-message
/// vault-sync handshake exactly as production does).
#[allow(dead_code)] // see RelayNode — per-binary common compilation
pub async fn build_node_with_relay(
    seed_byte: u8,
    relay_url: &RelayUrl,
) -> anyhow::Result<RelayNode> {
    let fs = Arc::new(InMemoryFs::new());
    let author = PeerId::from_secret_bytes(seed(seed_byte));
    let vault = Vault::init(fs.clone(), author.as_u64()).await?;
    let vault = Arc::new(Mutex::new(vault));

    let allowlist = Arc::new(InMemoryAllowlist::new());
    let (inbound_seen_tx, inbound_seen_rx) = mpsc::unbounded_channel();
    let sync_handler = sync_daemon::daemon::PumpedSyncHandler::new(
        vault.clone(),
        allowlist.clone(),
        inbound_seen_tx,
    );
    let sync_node = SyncNode::new_with_sync_handler(
        seed(seed_byte),
        std::slice::from_ref(relay_url),
        allowlist.clone(),
        sync_handler,
    )
    .await?;
    let node_id = PeerId::from_bytes(*sync_node.node_id().as_bytes());

    Ok(RelayNode {
        vault,
        fs,
        allowlist,
        sync_node,
        node_id,
        inbound_seen_rx,
    })
}

/// Build a [`RelayNode`] whose home relay is `relay_url` and whose endpoint has
/// **no IP transports** — relay is the only routing path.
///
/// Identical to [`build_node_with_relay`] except it uses
/// [`SyncNode::new_relay_only`], reproducing the off-LAN / behind-NAT condition:
/// two such loopback nodes cannot fall back to direct addresses, so the relay is
/// genuinely the only way they reach each other. Without this, in-process
/// localhost nodes discover each other's direct addresses and bypass the relay,
/// masking relay-path bugs.
#[allow(dead_code)] // see RelayNode — per-binary common compilation
pub async fn build_relay_only_node(
    seed_byte: u8,
    relay_url: &RelayUrl,
) -> anyhow::Result<RelayNode> {
    let fs = Arc::new(InMemoryFs::new());
    let author = PeerId::from_secret_bytes(seed(seed_byte));
    let vault = Vault::init(fs.clone(), author.as_u64()).await?;
    let vault = Arc::new(Mutex::new(vault));

    let allowlist = Arc::new(InMemoryAllowlist::new());
    let (inbound_seen_tx, inbound_seen_rx) = mpsc::unbounded_channel();
    let sync_handler = sync_daemon::daemon::PumpedSyncHandler::new(
        vault.clone(),
        allowlist.clone(),
        inbound_seen_tx,
    );
    let sync_node = SyncNode::new_relay_only_with_sync_handler(
        seed(seed_byte),
        std::slice::from_ref(relay_url),
        allowlist.clone(),
        sync_handler,
    )
    .await?;
    let node_id = PeerId::from_bytes(*sync_node.node_id().as_bytes());

    Ok(RelayNode {
        vault,
        fs,
        allowlist,
        sync_node,
        node_id,
        inbound_seen_rx,
    })
}

/// Poll `predicate` until it returns true or `deadline` elapses, panicking on
/// timeout. Checks every 100ms.
///
/// Takes an explicit deadline rather than a fixed budget because the relay-reap
/// repro must outlast iroh's hardcoded 60s `RELAY_INACTIVE_CLEANUP_TIME` — far
/// longer than `daemon_integration.rs`'s 10s poller allows.
#[allow(dead_code)] // see RelayNode — per-binary common compilation
pub async fn wait_until<F, Fut>(description: &str, deadline: Duration, predicate: F)
where
    F: Fn() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    let deadline = tokio::time::Instant::now() + deadline;
    loop {
        if predicate().await {
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!("timed out waiting for: {description}");
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}
