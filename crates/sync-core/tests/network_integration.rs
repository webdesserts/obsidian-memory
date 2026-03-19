/// Integration tests for the sync-core network module.
///
/// Tests exercise `SyncNode` construction, vault gossip subscriptions, and
/// QUIC bi-stream sync request/response round-trips between two in-process nodes.
///
/// WASM: All tests are native-only — the iroh networking stack requires tokio
/// and real sockets.
#[cfg(feature = "native")]
mod network_integration {
    use std::collections::HashMap;
    use std::time::Duration;

    use iroh::{
        EndpointAddr, RelayMode,
        address_lookup::memory::MemoryLookup,
        endpoint::presets,
    };
    use iroh_gossip::{Gossip, net::GOSSIP_ALPN};
    use iroh::protocol::Router;
    use sync_core::network::{
        gossip::GossipEvent,
        streams::{connect_and_sync_raw, SyncStreamHandler},
        SyncNode,
    };
    use sync_core::network::SYNC_ALPN;
    use sync_core::sync::SyncMessage;
    use sync_core::peer_id::VaultId;

    // ── helpers ──────────────────────────────────────────────────────────────

    /// Generate a deterministic 32-byte key seed from a small integer.
    fn seed(n: u8) -> [u8; 32] {
        [n; 32]
    }

    /// Create a pair of `SyncNode`s that can reach each other directly (no relay).
    ///
    /// Returns `(node_a, node_b, addr_a, addr_b)`.
    async fn make_test_pair() -> anyhow::Result<(SyncNode, SyncNode, EndpointAddr, EndpointAddr)> {
        let node_a = make_test_node(seed(1)).await?;
        let node_b = make_test_node(seed(2)).await?;

        let addr_a = node_a.endpoint.addr();
        let addr_b = node_b.endpoint.addr();

        // Teach each node how to reach the other.
        let lookup_a = MemoryLookup::new();
        lookup_a.add_endpoint_info(addr_b.clone());
        node_a.endpoint.address_lookup()?.add(lookup_a);

        let lookup_b = MemoryLookup::new();
        lookup_b.add_endpoint_info(addr_a.clone());
        node_b.endpoint.address_lookup()?.add(lookup_b);

        Ok((node_a, node_b, addr_a, addr_b))
    }

    /// Build a `SyncNode` with relay disabled for local testing.
    async fn make_test_node(secret_key_bytes: [u8; 32]) -> anyhow::Result<SyncNode> {
        use ed25519_dalek::SigningKey;
        use iroh::{SecretKey, Endpoint};

        let signing_key = SigningKey::from_bytes(&secret_key_bytes);
        let secret_key = SecretKey::from_bytes(&signing_key.to_bytes());

        let endpoint = Endpoint::builder(presets::N0)
            .secret_key(secret_key)
            .relay_mode(RelayMode::Disabled)
            .bind()
            .await?;

        let gossip = Gossip::builder().spawn(endpoint.clone());
        let (sync_handler, inbound_sync_rx) = SyncStreamHandler::new();

        let router = Router::builder(endpoint.clone())
            .accept(GOSSIP_ALPN, gossip.clone())
            .accept(SYNC_ALPN.to_vec(), sync_handler)
            .spawn();

        Ok(SyncNode::new_from_parts(endpoint, gossip, inbound_sync_rx, router))
    }

    // ── SyncNode construction ─────────────────────────────────────────────────

    /// Nodes created from different seeds must have distinct `EndpointId`s.
    #[tokio::test]
    async fn sync_node_has_unique_id_per_key() -> anyhow::Result<()> {
        let node_a = make_test_node(seed(10)).await?;
        let node_b = make_test_node(seed(20)).await?;

        assert_ne!(node_a.node_id(), node_b.node_id());
        Ok(())
    }

    /// A node created from the same seed always produces the same `EndpointId`.
    #[tokio::test]
    async fn sync_node_deterministic_from_seed() -> anyhow::Result<()> {
        let node_a = make_test_node(seed(42)).await?;
        let node_b = make_test_node(seed(42)).await?;

        assert_eq!(node_a.node_id(), node_b.node_id());
        Ok(())
    }

    // ── vault_topic ───────────────────────────────────────────────────────────

    /// The same `VaultId` always yields the same `TopicId`.
    #[tokio::test]
    async fn vault_topic_is_deterministic() {
        let vault_id: VaultId = "a1b2c3d4e5f67890".parse().unwrap();
        let topic_a = SyncNode::vault_topic(&vault_id);
        let topic_b = SyncNode::vault_topic(&vault_id);
        assert_eq!(topic_a, topic_b);
    }

    /// Different `VaultId`s produce different `TopicId`s.
    #[tokio::test]
    async fn vault_topic_differs_per_vault() {
        let vault_a: VaultId = "a1b2c3d4e5f67890".parse().unwrap();
        let vault_b: VaultId = "b2c3d4e5f6789001".parse().unwrap();
        assert_ne!(
            SyncNode::vault_topic(&vault_a),
            SyncNode::vault_topic(&vault_b)
        );
    }

    // ── gossip ────────────────────────────────────────────────────────────────

    /// Two `SyncNode`s can exchange a gossip change notification.
    #[tokio::test]
    async fn two_nodes_can_exchange_gossip() -> anyhow::Result<()> {
        let (node_a, node_b, _addr_a, _addr_b) = make_test_pair().await?;

        let vault_id: VaultId = "deadbeefdeadbeef".parse().unwrap();
        let node_b_id = node_b.node_id();

        // A subscribes with no bootstrap peers — it waits for B to join.
        let mut gossip_a = node_a
            .join_vault_gossip(&vault_id, vec![])
            .await?;

        // B subscribes and bootstraps off A.
        let mut gossip_b = node_b
            .join_vault_gossip(&vault_id, vec![node_a.node_id()])
            .await?;

        // Wait for B to connect to A.
        let neighbor = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match gossip_b.event_rx.recv().await {
                    Some(GossipEvent::NeighborUp(id)) => break id,
                    Some(_) => continue,
                    None => panic!("gossip_b event channel closed"),
                }
            }
        })
        .await
        .expect("timed out waiting for B's NeighborUp");

        assert_eq!(neighbor, node_a.node_id(), "B's NeighborUp should identify A");

        // B broadcasts a change notification.
        gossip_b.broadcast_change("notes/hello.md").await?;

        // A should receive it.
        let received = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match gossip_a.event_rx.recv().await {
                    Some(GossipEvent::ChangeReceived { from, notification }) => {
                        break (from, notification.path);
                    }
                    Some(_) => continue,
                    None => panic!("gossip_a event channel closed"),
                }
            }
        })
        .await
        .expect("timed out waiting for A to receive change");

        assert_eq!(received.0, node_b_id, "received.from should be B's id");
        assert_eq!(received.1, "notes/hello.md");

        Ok(())
    }

    // ── QUIC bi-stream sync round-trip ────────────────────────────────────────

    /// A client node can open a QUIC bi-stream to a server node, send raw
    /// sync bytes, and receive raw response bytes back.
    ///
    /// The test serializes `SyncMessage` values manually to mirror how the
    /// vault layer uses the raw transport.
    #[tokio::test]
    async fn sync_request_response_round_trips() -> anyhow::Result<()> {
        let (node_server, node_client, _addr_server, _addr_client) = make_test_pair().await?;
        let addr_server = node_server.endpoint.addr();

        // Drive the server's inbound request handler in a background task.
        // For each inbound request, respond with a fixed SyncResponse (raw bytes).
        let expected_response = SyncMessage::SyncResponse {
            registry_updates: None,
            document_updates: HashMap::from([(
                "notes/hello.md".to_string(),
                b"update-bytes".to_vec(),
            )]),
        };
        let response_bytes = bincode::serialize(&expected_response)?;
        let response_bytes_clone = response_bytes.clone();

        let mut inbound_rx = node_server.inbound_sync_rx;
        tokio::spawn(async move {
            while let Some(req) = inbound_rx.recv().await {
                let _ = req.reply_tx.send(response_bytes_clone.clone());
            }
        });

        // Client sends a SyncRequest as raw bytes.
        let request = SyncMessage::SyncRequest {
            registry_version: vec![1, 2, 3],
            document_versions: HashMap::from([("notes/hello.md".to_string(), vec![0u8; 4])]),
        };
        let request_bytes = bincode::serialize(&request)?;

        let received_bytes = tokio::time::timeout(
            Duration::from_secs(10),
            connect_and_sync_raw(&node_client.endpoint, addr_server, &request_bytes),
        )
        .await
        .expect("timed out waiting for sync response")?;

        // Decode the raw response and verify it matches what the server sent.
        let response: SyncMessage = bincode::deserialize(&received_bytes)?;
        match (response, expected_response) {
            (
                SyncMessage::SyncResponse {
                    registry_updates: ra,
                    document_updates: da,
                },
                SyncMessage::SyncResponse {
                    registry_updates: rb,
                    document_updates: db,
                },
            ) => {
                assert_eq!(ra, rb);
                assert_eq!(da, db);
            }
            _ => panic!("Expected SyncResponse"),
        }

        Ok(())
    }

    /// Sending a large message (1 MiB of document data) succeeds.
    #[tokio::test]
    async fn sync_large_message_round_trips() -> anyhow::Result<()> {
        let (node_server, node_client, _addr_server, _addr_client) = make_test_pair().await?;
        let addr_server = node_server.endpoint.addr();

        let large_data = vec![0xABu8; 1024 * 1024];
        let response_msg = SyncMessage::SyncResponse {
            registry_updates: None,
            document_updates: HashMap::from([("big.md".to_string(), large_data)]),
        };
        let response_bytes = bincode::serialize(&response_msg)?;
        let response_bytes_clone = response_bytes.clone();

        let mut inbound_rx = node_server.inbound_sync_rx;
        tokio::spawn(async move {
            while let Some(req) = inbound_rx.recv().await {
                let _ = req.reply_tx.send(response_bytes_clone.clone());
            }
        });

        let request = SyncMessage::SyncRequest {
            registry_version: vec![],
            document_versions: HashMap::new(),
        };
        let request_bytes = bincode::serialize(&request)?;

        let received_bytes = tokio::time::timeout(
            Duration::from_secs(15),
            connect_and_sync_raw(&node_client.endpoint, addr_server, &request_bytes),
        )
        .await
        .expect("timed out")?;

        let response: SyncMessage = bincode::deserialize(&received_bytes)?;
        match response {
            SyncMessage::SyncResponse { document_updates, .. } => {
                let data = document_updates.get("big.md").expect("missing big.md");
                assert_eq!(data.len(), 1024 * 1024);
                assert!(data.iter().all(|&b| b == 0xAB));
            }
            _ => panic!("Expected SyncResponse"),
        }

        Ok(())
    }

    /// Multiple sequential sync round-trips on the same pair of nodes succeed.
    #[tokio::test]
    async fn multiple_sequential_sync_round_trips() -> anyhow::Result<()> {
        let (node_server, node_client, _addr_server, _addr_client) = make_test_pair().await?;
        let addr_server = node_server.endpoint.addr();

        let mut inbound_rx = node_server.inbound_sync_rx;
        tokio::spawn(async move {
            while let Some(req) = inbound_rx.recv().await {
                let response = SyncMessage::SyncResponse {
                    registry_updates: None,
                    document_updates: HashMap::new(),
                };
                let bytes = bincode::serialize(&response).expect("serialization failed");
                let _ = req.reply_tx.send(bytes);
            }
        });

        for i in 0u8..3 {
            let request = SyncMessage::SyncRequest {
                registry_version: vec![i],
                document_versions: HashMap::new(),
            };
            let request_bytes = bincode::serialize(&request)?;

            let received_bytes = tokio::time::timeout(
                Duration::from_secs(10),
                connect_and_sync_raw(&node_client.endpoint, addr_server.clone(), &request_bytes),
            )
            .await
            .expect("timed out")?;

            let response: SyncMessage = bincode::deserialize(&received_bytes)?;
            assert!(
                matches!(response, SyncMessage::SyncResponse { .. }),
                "round trip {i}: expected SyncResponse"
            );
        }

        Ok(())
    }

    /// `broadcast_allowlist_update` sends an `AllowlistUpdate` gossip message that
    /// a subscribed peer receives and deserializes correctly.
    ///
    /// This mirrors the post-pairing flow where the mesh member broadcasts the
    /// new peer's identity to all other members so they can update their allowlists.
    #[tokio::test]
    async fn broadcast_allowlist_update_round_trips() -> anyhow::Result<()> {
        use sync_core::allowlist::AllowedPeer;
        use sync_core::peer_id::PeerId;
        use sync_core::network::gossip::GossipEvent;

        // Use unique seeds to avoid key collisions with other tests running concurrently.
        let node_a = make_test_node(seed(50)).await?;
        let node_b = make_test_node(seed(51)).await?;

        let addr_a = node_a.endpoint.addr();
        let addr_b = node_b.endpoint.addr();

        let lookup_a = MemoryLookup::new();
        lookup_a.add_endpoint_info(addr_b.clone());
        node_a.endpoint.address_lookup()?.add(lookup_a);

        let lookup_b = MemoryLookup::new();
        lookup_b.add_endpoint_info(addr_a.clone());
        node_b.endpoint.address_lookup()?.add(lookup_b);

        let vault_id: VaultId = "cafebabecafebabe".parse().unwrap();
        let node_b_id = node_b.node_id();

        // A subscribes with no bootstrap peers — it waits for B to join.
        let mut gossip_a = node_a.join_vault_gossip(&vault_id, vec![]).await?;

        // B subscribes and bootstraps off A.
        let mut gossip_b = node_b
            .join_vault_gossip(&vault_id, vec![node_a.node_id()])
            .await?;

        // Wait for B to connect to A.
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match gossip_b.event_rx.recv().await {
                    Some(GossipEvent::NeighborUp(_)) => break,
                    Some(_) => continue,
                    None => panic!("gossip_b event channel closed"),
                }
            }
        })
        .await
        .expect("timed out waiting for B's NeighborUp");

        // Test connectivity with ChangeReceived first
        gossip_b.broadcast_change("test.md").await?;

        // A should receive the ChangeReceived event.
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match gossip_a.event_rx.recv().await {
                    Some(GossipEvent::ChangeReceived { .. }) => break,
                    Some(_) => continue,
                    None => panic!("gossip_a event channel closed"),
                }
            }
        })
        .await
        .expect("timed out waiting for A to receive ChangeReceived");

        // B broadcasts an allowlist update for a newly-paired peer.
        let new_peer_id = PeerId::from_bytes([0x42u8; 32]);
        let new_peer = AllowedPeer::new(new_peer_id, "new-device");
        gossip_b.broadcast_allowlist_update(&new_peer).await?;

        // A should receive the AllowlistUpdate event.
        let received = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match gossip_a.event_rx.recv().await {
                    Some(GossipEvent::AllowlistUpdate { from, peer }) => break (from, peer),
                    Some(_) => continue,
                    None => panic!("gossip_a event channel closed"),
                }
            }
        })
        .await
        .expect("timed out waiting for A to receive AllowlistUpdate");

        assert_eq!(received.0, node_b_id, "AllowlistUpdate.from should be B's id");
        assert_eq!(received.1.node_id, new_peer_id, "AllowlistUpdate.peer should match");
        assert_eq!(received.1.device_name, "new-device");

        Ok(())
    }
}
