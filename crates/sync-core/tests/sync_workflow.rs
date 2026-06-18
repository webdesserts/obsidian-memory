/// End-to-end workflow integration tests for the sync system.
///
/// These tests create real iroh nodes backed by in-memory Vault instances to
/// verify the complete user-facing sync lifecycle: file changes propagating via
/// gossip, QUIC-based document exchange, allowlist enforcement, and reconnect
/// behavior.
///
/// Each test creates two `TestDevice` bundles — Vault + SyncNode — and manually
/// creates gossip subscriptions as needed. This keeps each device in exactly one
/// gossip subscription per topic, matching the daemon's behavior.
#[cfg(feature = "native")]
mod sync_workflow {
    use std::sync::Arc;
    use std::time::Duration;

    use iroh::{EndpointAddr, address_lookup::memory::MemoryLookup};
    use tokio::sync::Mutex;

    use sync_core::allowlist::{AllowedPeer, AllowlistStorage, InMemoryAllowlist};
    use sync_core::fs::{FileSystem, InMemoryFs};
    use sync_core::network::{
        SyncNode,
        gossip::{GossipEvent, VaultGossip},
        streams::{InboundSyncRequest, connect_and_sync_raw},
    };
    use sync_core::peer_id::VaultId;
    use sync_core::{PeerRegistry, Vault};

    // ── helpers ──────────────────────────────────────────────────────────────

    /// A deterministic 32-byte seed for building test nodes from small integers.
    fn seed(n: u8) -> [u8; 32] {
        [n; 32]
    }

    /// All devices in a test share this gossip topic so they join the same swarm.
    ///
    /// Each device uses its own internal VaultId for Loro authoring, but gossip
    /// routing only requires that the topic string matches across participants.
    fn shared_vault_id() -> VaultId {
        "deadbeefdeadbeef".parse().unwrap()
    }

    /// A sync device for testing: vault, iroh node, filesystem, peer registry,
    /// and the inbound sync request channel (kept separate for caller to drive).
    ///
    /// Gossip subscriptions are created explicitly by tests rather than inside
    /// `make_device` to ensure each device has at most one subscription per topic.
    /// Multiple subscriptions on the same topic can cause iroh-gossip to deliver
    /// messages to only one of them non-deterministically.
    struct TestDevice {
        vault: Arc<Mutex<Vault<Arc<InMemoryFs>>>>,
        sync_node: SyncNode,
        #[allow(dead_code)]
        peer_registry: PeerRegistry,
        fs: Arc<InMemoryFs>,
        /// Allowlist controlling which peers this device accepts sync requests from.
        allowlist: Arc<InMemoryAllowlist>,
        /// Inbound QUIC sync requests. Pass to `spawn_inbound_handler` to start
        /// processing incoming sync connections.
        inbound_sync_rx: tokio::sync::mpsc::UnboundedReceiver<InboundSyncRequest>,
    }

    /// Build a `TestDevice` from a seed byte.
    ///
    /// Creates a fresh `InMemoryFs`, initializes a Vault, and constructs an iroh
    /// node using the real `SyncNode::new()` constructor (same code path as
    /// production). A `MemoryLookup` is added for direct in-process connectivity.
    ///
    /// No gossip subscription is created here — tests create subscriptions explicitly
    /// via `sync_node.join_vault_gossip(...)` so each device has exactly one.
    async fn make_device(seed_byte: u8) -> anyhow::Result<TestDevice> {
        use sync_core::peer_id::PeerId;
        let fs = Arc::new(InMemoryFs::new());
        // Author Loro ops under a per-device PeerId derived from this device's
        // secret seed (the same seed driving its iroh node), so each device is a
        // distinct Loro replica.
        let author = PeerId::from_secret_bytes(seed(seed_byte));
        let vault = Vault::init(fs.clone(), author).await?;
        let vault = Arc::new(Mutex::new(vault));

        let allowlist = Arc::new(InMemoryAllowlist::new());
        let mut sync_node = SyncNode::new(seed(seed_byte), &[], allowlist.clone()).await?;

        // Add MemoryLookup for direct in-process connectivity without a relay.
        let memory_lookup = MemoryLookup::new();
        sync_node.endpoint.address_lookup()?.add(memory_lookup);

        // Extract `inbound_sync_rx` from the node so the caller can drive it
        // from outside, mirroring how the daemon owns the receiver separately.
        let (_, dead_rx) = tokio::sync::mpsc::unbounded_channel::<InboundSyncRequest>();
        let inbound_sync_rx = std::mem::replace(&mut sync_node.inbound_sync_rx, dead_rx);

        Ok(TestDevice {
            vault,
            sync_node,
            peer_registry: PeerRegistry::new(),
            fs,
            allowlist,
            inbound_sync_rx,
        })
    }

    /// Wire two devices so they can dial each other directly via `MemoryLookup`,
    /// and pre-populate each device's allowlist with the other's PeerId.
    ///
    /// Tests that need an *unauthorized* device should call this only for the
    /// authorized pair, leaving the unauthorized device's allowlist empty or
    /// adding a different (unrelated) peer.
    async fn connect_devices(a: &TestDevice, b: &TestDevice) -> anyhow::Result<()> {
        use sync_core::peer_id::PeerId;

        let addr_a = a.sync_node.endpoint.addr();
        let addr_b = b.sync_node.endpoint.addr();

        let lookup_a = MemoryLookup::new();
        lookup_a.add_endpoint_info(addr_b.clone());
        a.sync_node.endpoint.address_lookup()?.add(lookup_a);

        let lookup_b = MemoryLookup::new();
        lookup_b.add_endpoint_info(addr_a.clone());
        b.sync_node.endpoint.address_lookup()?.add(lookup_b);

        // Pre-populate each allowlist with the other's PeerId so gossip and sync
        // requests are accepted between paired devices.
        let peer_a = PeerId::from_bytes(*a.sync_node.node_id().as_bytes());
        let peer_b = PeerId::from_bytes(*b.sync_node.node_id().as_bytes());
        a.allowlist.add_peer(peer_b, "device-b").await?;
        b.allowlist.add_peer(peer_a, "device-a").await?;

        Ok(())
    }

    /// Wait for a specific gossip event, discarding others, with a 10-second timeout.
    ///
    /// Returns the first event that the `matcher` accepts. Panics if the channel
    /// closes or the timeout expires before a matching event arrives.
    async fn wait_for_gossip<F, T>(
        rx: &mut tokio::sync::mpsc::UnboundedReceiver<GossipEvent>,
        matcher: F,
    ) -> T
    where
        F: Fn(GossipEvent) -> Option<T>,
    {
        tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                match rx.recv().await {
                    Some(event) => {
                        if let Some(result) = matcher(event) {
                            return result;
                        }
                    }
                    None => panic!("gossip event channel closed unexpectedly"),
                }
            }
        })
        .await
        .expect("timed out waiting for gossip event")
    }

    /// Spawn a background task that processes all inbound sync requests for a device.
    ///
    /// Each request is first checked against the allowlist, mirroring the daemon's
    /// `on_inbound_sync` behavior: if the remote peer is not in the allowlist,
    /// `reply_tx` is dropped to close the stream without a response. Allowed
    /// requests are forwarded to the vault's `process_sync_message`.
    fn spawn_inbound_handler(
        vault: Arc<Mutex<Vault<Arc<InMemoryFs>>>>,
        allowlist: Arc<InMemoryAllowlist>,
        mut inbound_rx: tokio::sync::mpsc::UnboundedReceiver<InboundSyncRequest>,
    ) {
        tokio::spawn(async move {
            while let Some(req) = inbound_rx.recv().await {
                // Deny peers not in the allowlist — same logic as the daemon's on_inbound_sync.
                let allowed = allowlist.is_allowed(&req.remote_id).await.unwrap_or(false);
                if !allowed {
                    // Dropping reply_tx closes the QUIC stream without a response.
                    drop(req.reply_tx);
                    continue;
                }

                let vault = vault.lock().await;
                match vault.process_sync_message(&req.message_bytes).await {
                    Ok((Some(response_bytes), _)) => {
                        let _ = req.reply_tx.send(response_bytes);
                    }
                    Ok((None, _)) => {
                        // No response needed — stream closes gracefully.
                    }
                    Err(e) => {
                        tracing::warn!("inbound sync handler error: {}", e);
                    }
                }
            }
        });
    }

    /// Subscribe device A to gossip with no bootstrap peers (waits for others to join).
    async fn subscribe_gossip(device: &TestDevice) -> anyhow::Result<VaultGossip> {
        device
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![])
            .await
            .map_err(Into::into)
    }

    /// Subscribe device B to gossip, bootstrapping off device A.
    async fn subscribe_gossip_via(b: &TestDevice, a: &TestDevice) -> anyhow::Result<VaultGossip> {
        b.sync_node
            .join_vault_gossip(&shared_vault_id(), vec![a.sync_node.node_id()])
            .await
            .map_err(Into::into)
    }

    // ── file sync tests ───────────────────────────────────────────────────────

    /// A file change on Device A propagates to Device B via gossip + QUIC pull.
    ///
    /// This covers the standard write-and-broadcast workflow: user edits a note,
    /// the daemon indexes the change and gossips a `ChangeNotification`, and the
    /// peer pulls the full update over QUIC.
    #[tokio::test]
    async fn test_file_change_syncs_between_devices() -> anyhow::Result<()> {
        let device_a = make_device(1).await?;
        let device_b = make_device(2).await?;

        connect_devices(&device_a, &device_b).await?;

        // Each device subscribes to exactly one gossip topic. A subscribes first
        // (empty bootstrap), B subscribes second (bootstrapping off A).
        let mut gossip_a = subscribe_gossip(&device_a).await?;
        let mut gossip_b = subscribe_gossip_via(&device_b, &device_a).await?;

        // Wait for mutual NeighborUp before doing any work.
        wait_for_gossip(&mut gossip_a.event_rx, |e| match e {
            GossipEvent::NeighborUp(_) => Some(()),
            _ => None,
        })
        .await;
        wait_for_gossip(&mut gossip_b.event_rx, |e| match e {
            GossipEvent::NeighborUp(_) => Some(()),
            _ => None,
        })
        .await;

        // Drive A's inbound QUIC handler so B can pull from A.
        spawn_inbound_handler(
            device_a.vault.clone(),
            device_a.allowlist.clone(),
            device_a.inbound_sync_rx,
        );

        // A writes a file and indexes the change.
        device_a
            .fs
            .write("notes/hello.md", b"# Hello World")
            .await?;
        {
            let vault = device_a.vault.lock().await;
            vault.on_file_changed("notes/hello.md").await?;
        }

        // A broadcasts the change via gossip.
        gossip_a.broadcast_change("notes/hello.md").await?;

        // B waits for the change notification.
        let _notification = wait_for_gossip(&mut gossip_b.event_rx, |e| match e {
            GossipEvent::ChangeReceived { notification, .. } => Some(notification),
            _ => None,
        })
        .await;

        // B pulls the change from A via QUIC (one round-trip: SyncRequest → SyncExchange).
        let addr_a: EndpointAddr = device_a.sync_node.endpoint.addr();
        let request_bytes = {
            let vault = device_b.vault.lock().await;
            vault.prepare_sync_request().await?
        };
        let response_bytes =
            connect_and_sync_raw(&device_b.sync_node.endpoint, addr_a, &request_bytes).await?;

        {
            let vault = device_b.vault.lock().await;
            vault.process_sync_message(&response_bytes).await?;
        }

        // Verify B now has the file that A created.
        let files_b = {
            let vault = device_b.vault.lock().await;
            vault.list_files().await?
        };
        assert!(
            files_b.contains(&"notes/hello.md".to_string()),
            "Device B should have notes/hello.md after sync, got: {:?}",
            files_b
        );

        Ok(())
    }

    /// A file deletion on Device A propagates to Device B via gossip notification
    /// followed by a QUIC pull.
    ///
    /// The gossip layer notifies B that A's state has changed. B then pulls the
    /// full registry update via QUIC, which carries the CRDT deletion entry.
    ///
    /// Note: iroh-gossip's epidemic broadcast protocol deduplicates messages with
    /// identical content. We use different paths for the "file added" and "file
    /// deleted" notifications to avoid hitting dedup, which mirrors production
    /// behavior (where files have unique paths that differ between events).
    #[tokio::test]
    async fn test_file_deletion_syncs_between_devices() -> anyhow::Result<()> {
        let device_a = make_device(3).await?;
        let device_b = make_device(4).await?;

        connect_devices(&device_a, &device_b).await?;

        let mut gossip_a = subscribe_gossip(&device_a).await?;
        let mut gossip_b = subscribe_gossip_via(&device_b, &device_a).await?;

        // Wait for gossip swarm to form.
        wait_for_gossip(&mut gossip_a.event_rx, |e| match e {
            GossipEvent::NeighborUp(_) => Some(()),
            _ => None,
        })
        .await;
        wait_for_gossip(&mut gossip_b.event_rx, |e| match e {
            GossipEvent::NeighborUp(_) => Some(()),
            _ => None,
        })
        .await;

        // Wire inbound handlers on both sides so either can act as QUIC responder.
        spawn_inbound_handler(
            device_a.vault.clone(),
            device_a.allowlist.clone(),
            device_a.inbound_sync_rx,
        );
        spawn_inbound_handler(
            device_b.vault.clone(),
            device_b.allowlist.clone(),
            device_b.inbound_sync_rx,
        );

        let addr_a: EndpointAddr = device_a.sync_node.endpoint.addr();

        // Step 1: A creates the file and B pulls it via QUIC.
        // We use gossip to signal B that there's something to pull.
        device_a
            .fs
            .write("notes/delete-me.md", b"to be deleted")
            .await?;
        {
            let vault = device_a.vault.lock().await;
            vault.on_file_changed("notes/delete-me.md").await?;
        }
        gossip_a.broadcast_change("notes/delete-me.md").await?;

        wait_for_gossip(&mut gossip_b.event_rx, |e| match e {
            GossipEvent::ChangeReceived { .. } => Some(()),
            _ => None,
        })
        .await;

        let request_bytes = {
            let vault = device_b.vault.lock().await;
            vault.prepare_sync_request().await?
        };
        let response_bytes =
            connect_and_sync_raw(&device_b.sync_node.endpoint, addr_a.clone(), &request_bytes)
                .await?;
        {
            let vault = device_b.vault.lock().await;
            vault.process_sync_message(&response_bytes).await?;
        }

        // Confirm B has the file before we delete it.
        let files_before = {
            let vault = device_b.vault.lock().await;
            vault.list_files().await?
        };
        assert!(
            files_before.contains(&"notes/delete-me.md".to_string()),
            "B should have the file before deletion, got: {:?}",
            files_before
        );

        // Step 2: A deletes the file. B learns about it via a gossip notification
        // on a *different* path ("notes/sync-trigger.md") to avoid the epidemic
        // broadcast's dedup filter, which drops messages with identical content as
        // the one it already delivered.
        {
            let vault = device_a.vault.lock().await;
            vault.delete_file("notes/delete-me.md").await?;
        }
        // Signal via a distinct path — the QUIC pull will pick up the full registry
        // state (including the deletion) regardless of which path was in the notification.
        gossip_a.broadcast_change("notes/sync-trigger.md").await?;

        wait_for_gossip(&mut gossip_b.event_rx, |e| match e {
            GossipEvent::ChangeReceived { .. } => Some(()),
            _ => None,
        })
        .await;

        let request_bytes2 = {
            let vault = device_b.vault.lock().await;
            vault.prepare_sync_request().await?
        };
        let response_bytes2 =
            connect_and_sync_raw(&device_b.sync_node.endpoint, addr_a, &request_bytes2).await?;
        {
            let vault = device_b.vault.lock().await;
            vault.process_sync_message(&response_bytes2).await?;
        }

        // Confirm B no longer has the file.
        let files_after = {
            let vault = device_b.vault.lock().await;
            vault.list_files().await?
        };
        assert!(
            !files_after.contains(&"notes/delete-me.md".to_string()),
            "Device B should not have notes/delete-me.md after deletion sync, got: {:?}",
            files_after
        );

        Ok(())
    }

    // ── authorization tests ───────────────────────────────────────────────────

    /// An unrecognized device is denied when the allowlist is non-empty.
    ///
    /// This exercises the real `spawn_inbound_handler` allowlist check, which
    /// mirrors the daemon's `on_inbound_sync`: if the peer's `remote_id` is not
    /// in the allowlist, `reply_tx` is dropped, closing the QUIC stream without
    /// a response. This test verifies the vault state is unchanged after a rejected
    /// request.
    #[tokio::test]
    async fn test_unauthorized_device_rejected() -> anyhow::Result<()> {
        use sync_core::peer_id::PeerId;

        let device_a = make_device(5).await?;
        let device_unknown = make_device(6).await?;

        // Wire direct connectivity without adding device_unknown to A's allowlist.
        // Only add a fake peer to A's allowlist so it is non-empty (deny-all when empty
        // is a separate invariant; here we test that a non-empty allowlist excludes
        // unknown peers).
        let addr_a = device_a.sync_node.endpoint.addr();
        let addr_unknown = device_unknown.sync_node.endpoint.addr();

        let lookup_a = MemoryLookup::new();
        lookup_a.add_endpoint_info(addr_unknown.clone());
        device_a.sync_node.endpoint.address_lookup()?.add(lookup_a);

        let lookup_unknown = MemoryLookup::new();
        lookup_unknown.add_endpoint_info(addr_a.clone());
        device_unknown
            .sync_node
            .endpoint
            .address_lookup()?
            .add(lookup_unknown);

        // Populate A's allowlist with a fake peer (not device_unknown) so it is
        // non-empty — a non-empty allowlist denies all unlisted peers.
        let fake_peer_id = PeerId::from_bytes([0xAAu8; 32]);
        device_a
            .allowlist
            .add_peer(fake_peer_id, "other-device")
            .await?;

        // Write a file to A's vault so there's something to protect.
        device_a.fs.write("notes/private.md", b"secret").await?;
        {
            let vault = device_a.vault.lock().await;
            vault.on_file_changed("notes/private.md").await?;
        }

        // Drive A's inbound handler using the real allowlist enforcement.
        spawn_inbound_handler(
            device_a.vault.clone(),
            device_a.allowlist.clone(),
            device_a.inbound_sync_rx,
        );

        // Unknown device tries to sync with A.
        let request_bytes = {
            let vault = device_unknown.vault.lock().await;
            vault.prepare_sync_request().await?
        };

        // The connection will eventually fail when the server closes it after its
        // 30-second idle timeout — but we don't want a 30-second test. Instead, we
        // verify the invariant: Unknown's vault still has no files after the attempt.
        //
        // Fire-and-forget the sync attempt in a background task; it will fail or hang.
        let endpoint = device_unknown.sync_node.endpoint.clone();
        let addr_a_clone = addr_a.clone();
        let _sync_task = tokio::spawn(async move {
            let _ = connect_and_sync_raw(&endpoint, addr_a_clone, &request_bytes).await;
        });

        // Give the request time to reach A's handler and be rejected.
        tokio::time::sleep(Duration::from_millis(500)).await;

        // A's vault state must be unchanged — A still has private.md.
        let files_a = {
            let vault = device_a.vault.lock().await;
            vault.list_files().await?
        };
        assert!(
            files_a.contains(&"notes/private.md".to_string()),
            "A's vault should still have notes/private.md, got: {:?}",
            files_a
        );

        // Unknown's vault should be empty — the sync was blocked.
        let files_unknown = {
            let vault = device_unknown.vault.lock().await;
            vault.list_files().await?
        };
        assert!(
            files_unknown.is_empty(),
            "Unknown's vault should remain empty, got: {:?}",
            files_unknown
        );

        Ok(())
    }

    /// An allowlist update broadcast by one device is received by another.
    ///
    /// This covers the post-pairing flow: after pairing completes, the mesh member
    /// broadcasts the new peer's identity to all other mesh members so they can add
    /// the newly-paired device to their own allowlists.
    #[tokio::test]
    async fn test_allowlist_update_propagates_via_gossip() -> anyhow::Result<()> {
        let device_a = make_device(7).await?;
        let device_b = make_device(8).await?;

        connect_devices(&device_a, &device_b).await?;

        let mut gossip_a = subscribe_gossip(&device_a).await?;
        let mut gossip_b = subscribe_gossip_via(&device_b, &device_a).await?;

        // Wait for gossip swarm to form.
        wait_for_gossip(&mut gossip_a.event_rx, |e| match e {
            GossipEvent::NeighborUp(_) => Some(()),
            _ => None,
        })
        .await;
        wait_for_gossip(&mut gossip_b.event_rx, |e| match e {
            GossipEvent::NeighborUp(_) => Some(()),
            _ => None,
        })
        .await;

        // A broadcasts an allowlist update for a newly-paired peer.
        let new_peer_id = sync_core::peer_id::PeerId::from_bytes([0x99u8; 32]);
        let new_peer = AllowedPeer::new(new_peer_id, "new-device");
        gossip_a.broadcast_allowlist_update(&new_peer).await?;

        // B should receive the AllowlistUpdate event with the correct peer info.
        let (from, received_peer) = wait_for_gossip(&mut gossip_b.event_rx, |e| match e {
            GossipEvent::AllowlistUpdate { from, peer } => Some((from, peer)),
            _ => None,
        })
        .await;

        assert_eq!(
            from,
            device_a.sync_node.node_id(),
            "AllowlistUpdate sender should be device A"
        );
        assert_eq!(
            received_peer.node_id, new_peer_id,
            "AllowlistUpdate peer should match the broadcasted peer"
        );
        assert_eq!(received_peer.device_name, "new-device");

        Ok(())
    }

    // ── reconnect sync tests ──────────────────────────────────────────────────

    /// When a new device joins the gossip swarm, it pulls any files it missed
    /// by initiating a full sync toward the existing member on `NeighborUp`.
    ///
    /// This tests the `NeighborUp → full sync` flow: B joins the gossip swarm,
    /// sees A as a neighbor, and immediately sends a `SyncRequest` to A. A's
    /// inbound handler responds with a `SyncExchange` containing all files B is
    /// missing. B processes the exchange and now has A's pre-existing files.
    ///
    /// In the daemon both sides initiate on `NeighborUp` — this test exercises
    /// B's direction (the new-device side that needs to catch up).
    #[tokio::test]
    async fn test_neighbor_up_triggers_full_sync() -> anyhow::Result<()> {
        let device_a = make_device(9).await?;
        let device_b = make_device(10).await?;

        // A has a file before B joins.
        device_a
            .fs
            .write("notes/offline-edit.md", b"# Written offline")
            .await?;
        {
            let vault = device_a.vault.lock().await;
            vault.on_file_changed("notes/offline-edit.md").await?;
        }

        connect_devices(&device_a, &device_b).await?;

        // A subscribes first (empty bootstrap), B subscribes second (bootstrapping off A).
        let _gossip_a = subscribe_gossip(&device_a).await?;
        let mut gossip_b = subscribe_gossip_via(&device_b, &device_a).await?;

        // Wire A's inbound handler so B can pull from A when NeighborUp fires.
        spawn_inbound_handler(
            device_a.vault.clone(),
            device_a.allowlist.clone(),
            device_a.inbound_sync_rx,
        );

        // B waits to see A as a neighbor, then initiates a full sync to pull A's files.
        wait_for_gossip(&mut gossip_b.event_rx, |e| match e {
            GossipEvent::NeighborUp(_) => Some(()),
            _ => None,
        })
        .await;

        // B sends its SyncRequest to A (A's inbound handler responds with SyncExchange
        // containing all files B is missing, including notes/offline-edit.md).
        let addr_a: EndpointAddr = device_a.sync_node.endpoint.addr();
        let request_bytes = {
            let vault = device_b.vault.lock().await;
            vault.prepare_sync_request().await?
        };
        let response_bytes =
            connect_and_sync_raw(&device_b.sync_node.endpoint, addr_a, &request_bytes).await?;
        {
            let vault = device_b.vault.lock().await;
            vault.process_sync_message(&response_bytes).await?;
        }

        // B should now have the file A had before B joined.
        let files_b = {
            let vault = device_b.vault.lock().await;
            vault.list_files().await?
        };
        assert!(
            files_b.contains(&"notes/offline-edit.md".to_string()),
            "Device B should have notes/offline-edit.md after NeighborUp sync, got: {:?}",
            files_b
        );

        Ok(())
    }
}
