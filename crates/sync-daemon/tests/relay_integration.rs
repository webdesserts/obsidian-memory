//! Integration tests for the embedded relay server.
//!
//! These tests verify that two `SyncNode`s can discover and communicate with
//! each other through an `EmbeddedRelay` when they have no direct route.
//! Unlike the network_integration tests in sync-core (which use MemoryLookup
//! for direct connectivity), here the relay is the only path between nodes.
//!
//! sync-daemon is always native (no WASM target), so no cfg guards are needed.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::Duration;

use sync_core::allowlist::{AllowlistStorage, InMemoryAllowlist};
use sync_core::network::gossip::GossipEvent;
use sync_core::network::streams::connect_and_sync_raw;
use sync_core::network::SyncNode;
use sync_core::peer_id::{PeerId, VaultId};
use sync_core::sync::SyncMessage;
use sync_daemon::relay::EmbeddedRelay;

// ── helpers ───────────────────────────────────────────────────────────────

/// Generate a deterministic 32-byte key seed from a small integer.
fn seed(n: u8) -> [u8; 32] {
    [n; 32]
}

// ── tests ─────────────────────────────────────────────────────────────────

/// Two nodes configured with only an embedded relay as their routing path
/// can establish gossip connectivity and complete a QUIC sync round-trip.
///
/// This exercises the path where peers can't reach each other directly
/// (e.g., across NATs) and must relay traffic through the daemon's relay.
#[tokio::test]
async fn test_sync_through_embedded_relay() -> anyhow::Result<()> {
    // Start the relay on a random OS-assigned port.
    let bind_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let relay = EmbeddedRelay::start(bind_addr).await?;
    let relay_url = relay.relay_url().clone();

    // Derive each node's PeerId from its seed so we can pre-populate both allowlists.
    // Both nodes must allow each other for gossip connections to be accepted.
    let allowlist_a = Arc::new(InMemoryAllowlist::new());
    let allowlist_b = Arc::new(InMemoryAllowlist::new());

    // Create two nodes that only know about the relay — no direct address
    // exchange. They can only reach each other by routing through the relay.
    let node_a = SyncNode::new(seed(101), Some(&relay_url), allowlist_a.clone()).await?;
    let node_b = SyncNode::new(seed(102), Some(&relay_url), allowlist_b.clone()).await?;

    // Pre-populate each allowlist with the other node's PeerId so gossip is accepted.
    let peer_a = PeerId::from_bytes(*node_a.node_id().as_bytes());
    let peer_b = PeerId::from_bytes(*node_b.node_id().as_bytes());
    allowlist_a.add_peer(peer_b, "node-b").await?;
    allowlist_b.add_peer(peer_a, "node-a").await?;

    let vault_id: VaultId = "cafebabe0beef001".parse().unwrap();

    // Node A subscribes with no bootstrap peers — it waits for B.
    let mut gossip_a = node_a.join_vault_gossip(&vault_id, vec![]).await?;

    // Node B bootstraps off A's node ID (it knows A's identity but not its
    // direct address — the relay handles the actual routing).
    let mut gossip_b = node_b
        .join_vault_gossip(&vault_id, vec![node_a.node_id()])
        .await?;

    // Wait for B to establish a gossip neighbour relationship with A via relay.
    // Allow extra time since the relay adds a network hop.
    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match gossip_b.event_rx.recv().await {
                Some(GossipEvent::NeighborUp(id)) => {
                    assert_eq!(id, node_a.node_id(), "B's neighbor should be A");
                    break;
                }
                Some(_) => continue,
                None => panic!("gossip_b event channel closed before NeighborUp"),
            }
        }
    })
    .await
    .expect("timed out waiting for gossip NeighborUp through relay");

    // Perform a QUIC sync round-trip through the relay.
    // Drive the server's inbound handler in the background.
    let expected_response = SyncMessage::SyncResponse {
        registry_updates: None,
        document_updates: HashMap::from([(
            "notes/relay-test.md".to_string(),
            b"relay-sync-data".to_vec(),
        )]),
    };
    let response_bytes = bincode::serialize(&expected_response)?;
    let response_bytes_clone = response_bytes.clone();

    let mut inbound_rx = node_a.inbound_sync_rx;
    tokio::spawn(async move {
        while let Some(req) = inbound_rx.recv().await {
            let _ = req.reply_tx.send(response_bytes_clone.clone());
        }
    });

    // B connects to A's QUIC endpoint and sends a sync request.
    let addr_a = node_a.endpoint.addr();
    let request = SyncMessage::SyncRequest {
        registry_version: vec![1, 2, 3],
        document_versions: HashMap::from([(
            "notes/relay-test.md".to_string(),
            vec![0u8; 4],
        )]),
    };
    let request_bytes = bincode::serialize(&request)?;

    let received_bytes = tokio::time::timeout(
        Duration::from_secs(30),
        connect_and_sync_raw(&node_b.endpoint, addr_a, &request_bytes),
    )
    .await
    .expect("timed out waiting for QUIC sync response through relay")?;

    // Verify the response matches what the server sent.
    let response: SyncMessage = bincode::deserialize(&received_bytes)?;
    match response {
        SyncMessage::SyncResponse {
            registry_updates,
            document_updates,
        } => {
            assert!(registry_updates.is_none());
            let data = document_updates
                .get("notes/relay-test.md")
                .expect("missing relay-test.md in response");
            assert_eq!(data, b"relay-sync-data");
        }
        _ => panic!("expected SyncResponse, got something else"),
    }

    // Clean up.
    gossip_a.event_rx.close();
    relay.shutdown().await;

    Ok(())
}
