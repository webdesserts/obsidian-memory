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

use iroh::{EndpointId, SecretKey};
use sync_core::allowlist::{AllowlistStorage, InMemoryAllowlist};
use sync_core::network::SyncNode;
use sync_core::network::gossip::GossipEvent;
use sync_core::network::streams::connect_and_sync_raw;
use sync_core::peer_id::{PeerId, VaultId};
use sync_core::sync::SyncMessage;
use sync_daemon::relay::EmbeddedRelay;

mod common;

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
    let node_a = SyncNode::new(common::seed(101), Some(&relay_url), allowlist_a.clone()).await?;
    let node_b = SyncNode::new(common::seed(102), Some(&relay_url), allowlist_b.clone()).await?;

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
        document_versions: HashMap::from([("notes/relay-test.md".to_string(), vec![0u8; 4])]),
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

/// `add_peer_relay` registers a relay hint that is resolvable via the node's
/// peer lookup service.
///
/// This validates the seam used at startup when `DaemonConfig.peer_relays` is
/// non-empty: the hint is written to the `MemoryLookup` and can be queried
/// back by endpoint id before any live connection is attempted.
#[tokio::test]
async fn test_add_peer_relay_registers_resolvable_hint() -> anyhow::Result<()> {
    let allowlist = Arc::new(InMemoryAllowlist::new());
    let node = SyncNode::new(common::seed(10), None, allowlist).await?;

    // Use a second node's id as the "peer" and a standalone relay URL.
    let bind_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let relay = EmbeddedRelay::start(bind_addr).await?;
    let relay_url = relay.relay_url().clone();

    // A plausible peer id derived from a distinct key seed.
    let peer_secret = SecretKey::from_bytes(&[42u8; 32]);
    let peer_endpoint_id: EndpointId = peer_secret.public();

    // Before seeding, the lookup should have no entry for this peer.
    assert!(
        node.peer_lookup.get_endpoint_info(peer_endpoint_id).is_none(),
        "lookup should be empty before add_peer_relay"
    );

    node.add_peer_relay(peer_endpoint_id, &relay_url);

    // After seeding, the hint must be resolvable and carry the relay URL.
    let info = node
        .peer_lookup
        .get_endpoint_info(peer_endpoint_id)
        .expect("hint should be present after add_peer_relay");

    let relay_urls: Vec<_> = info.into_endpoint_addr().relay_urls().cloned().collect();
    assert_eq!(
        relay_urls,
        vec![relay_url],
        "seeded relay URL should be recoverable from the lookup"
    );

    relay.shutdown().await;
    Ok(())
}

/// `add_peer_relay` silently ignores a call where the target id equals the
/// node's own EndpointId, leaving the lookup empty.
///
/// iroh rejects self-directed relay paths, so seeding our own id into the
/// lookup is a footgun (`upsert_peer_relay` in persistence skips self, but
/// `add_peer_relay` is also called directly after pairing; this guard closes
/// the gap at the method level).
#[tokio::test]
async fn test_add_peer_relay_ignores_self_id() -> anyhow::Result<()> {
    let allowlist = Arc::new(InMemoryAllowlist::new());
    let node = SyncNode::new(common::seed(30), None, allowlist).await?;

    let bind_addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let relay = EmbeddedRelay::start(bind_addr).await?;
    let relay_url = relay.relay_url().clone();

    // Calling add_peer_relay with our own id should be a no-op.
    node.add_peer_relay(node.node_id(), &relay_url);

    // The lookup must remain empty — no self-hint was registered.
    assert!(
        node.peer_lookup.get_endpoint_info(node.node_id()).is_none(),
        "self-id should not be seeded into the peer lookup"
    );

    relay.shutdown().await;
    Ok(())
}

/// When `peer_relays` is empty at startup, the `MemoryLookup` service contains
/// no hints — the LAN-only path via mDNS is entirely unaffected.
///
/// This is the LAN-unchanged guard: confirms that the MemoryLookup registration
/// does not break or interfere with existing mDNS-based discovery when there
/// are no persisted relay hints to seed.
#[tokio::test]
async fn test_empty_peer_relays_leaves_lookup_empty() -> anyhow::Result<()> {
    let allowlist = Arc::new(InMemoryAllowlist::new());
    let node = SyncNode::new(common::seed(20), None, allowlist).await?;

    // A deterministic peer id derived from a key seed (distinct from the node's own seed).
    let peer_secret = SecretKey::from_bytes(&[99u8; 32]);
    let peer_endpoint_id: EndpointId = peer_secret.public();

    // With no add_peer_relay calls (simulating empty DaemonConfig.peer_relays),
    // the lookup must return None for any peer id.
    assert!(
        node.peer_lookup.get_endpoint_info(peer_endpoint_id).is_none(),
        "peer_lookup should be empty when no hints are seeded"
    );

    Ok(())
}

/// Per-peer relay hints resolve and route via a non-home relay.
///
/// **Topology:** Node A's home relay is R_a; Node B's home relay is R_b. The two
/// relay maps are disjoint — neither node has the other's relay in its `RelayMap`.
///
/// B's address lookup is seeded with `(A_id, R_a)` via `add_peer_relay`. B then
/// bootstraps gossip using A's bare `EndpointId` (no direct address). This exercises
/// the load-bearing path from the plan: lookup resolution → iroh spawning an
/// `ActiveRelayActor` for R_a (a URL not in B's RelayMap, handled on-demand per
/// iroh's non-home relay behavior) → routing via that relay. The mirror direction
/// (A's lookup seeded with B's relay) is also exercised in the same test.
///
/// **What this test proves:** hint resolution is wired up correctly; gossip and QUIC
/// sync succeed when the only pre-seeded path is via a non-home relay. It does NOT
/// prove that relay routing was the *exclusive* transport path used — in-process
/// localhost nodes can find each other via mDNS direct addresses, so iroh may use
/// direct QUIC as a faster path once discovered. The relay hint is available as a
/// bootstrap; whether iroh ultimately prefers it is an internal routing decision.
/// The true off-LAN property (both home relays unreachable to the dialer) is only
/// verifiable in the real-world charon ↔ umbra E2E scenario.
#[tokio::test]
async fn test_non_home_relay_hint_resolves_and_routes() -> anyhow::Result<()> {
    // Start two separate relays — R_a is A's home, R_b is B's home.
    let relay_a = EmbeddedRelay::start("127.0.0.1:0".parse().unwrap()).await?;
    let relay_b = EmbeddedRelay::start("127.0.0.1:0".parse().unwrap()).await?;
    let url_a = relay_a.relay_url().clone();
    let url_b = relay_b.relay_url().clone();

    // Pre-populate allowlists so each node accepts the other's gossip connections.
    let allowlist_a = Arc::new(InMemoryAllowlist::new());
    let allowlist_b = Arc::new(InMemoryAllowlist::new());

    // Node A: home relay = R_a (url_a is in its RelayMap).
    // Node B: home relay = R_b (url_b is in its RelayMap).
    // Neither node has the other's relay in its own RelayMap.
    let node_a = SyncNode::new(common::seed(51), Some(&url_a), allowlist_a.clone()).await?;
    let node_b = SyncNode::new(common::seed(52), Some(&url_b), allowlist_b.clone()).await?;

    let peer_a = PeerId::from_bytes(*node_a.node_id().as_bytes());
    let peer_b = PeerId::from_bytes(*node_b.node_id().as_bytes());
    allowlist_a.add_peer(peer_b, "node-b").await?;
    allowlist_b.add_peer(peer_a, "node-a").await?;

    // Seed per-peer relay hints in both directions.
    // B learns to reach A through R_a (not in B's RelayMap — non-home relay routing).
    node_b.add_peer_relay(node_a.node_id(), &url_a);
    // A learns to reach B through R_b (not in A's RelayMap — mirror direction).
    node_a.add_peer_relay(node_b.node_id(), &url_b);

    let vault_id: VaultId = "deadbeef00000001".parse().unwrap();

    // A subscribes with no bootstrap peers; B bootstraps from A's bare EndpointId.
    // The hint in B's lookup provides the relay path to A.
    let mut gossip_a = node_a.join_vault_gossip(&vault_id, vec![]).await?;
    let mut gossip_b = node_b
        .join_vault_gossip(&vault_id, vec![node_a.node_id()])
        .await?;

    // Assert NeighborUp on both sides.
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
    .expect("timed out waiting for B's NeighborUp");

    tokio::time::timeout(Duration::from_secs(30), async {
        loop {
            match gossip_a.event_rx.recv().await {
                Some(GossipEvent::NeighborUp(id)) => {
                    assert_eq!(id, node_b.node_id(), "A's neighbor should be B");
                    break;
                }
                Some(_) => continue,
                None => panic!("gossip_a event channel closed before NeighborUp"),
            }
        }
    })
    .await
    .expect("timed out waiting for A's NeighborUp");

    // Assert a QUIC sync round-trip (B→A direction).
    let expected_response = SyncMessage::SyncResponse {
        registry_updates: None,
        document_updates: HashMap::from([(
            "notes/relay-hint-test.md".to_string(),
            b"relay-hint-data".to_vec(),
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

    let addr_a = node_a.endpoint.addr();
    let request = SyncMessage::SyncRequest {
        registry_version: vec![1, 2, 3],
        document_versions: HashMap::from([(
            "notes/relay-hint-test.md".to_string(),
            vec![0u8; 4],
        )]),
    };
    let request_bytes = bincode::serialize(&request)?;

    let received_bytes = tokio::time::timeout(
        Duration::from_secs(30),
        connect_and_sync_raw(&node_b.endpoint, addr_a, &request_bytes),
    )
    .await
    .expect("timed out waiting for QUIC sync response")?;

    let response: SyncMessage = bincode::deserialize(&received_bytes)?;
    match response {
        SyncMessage::SyncResponse {
            registry_updates,
            document_updates,
        } => {
            assert!(registry_updates.is_none());
            let data = document_updates
                .get("notes/relay-hint-test.md")
                .expect("missing relay-hint-test.md in response");
            assert_eq!(data, b"relay-hint-data");
        }
        _ => panic!("expected SyncResponse, got something else"),
    }

    // Clean up.
    gossip_a.event_rx.close();
    relay_a.shutdown().await;
    relay_b.shutdown().await;

    Ok(())
}
