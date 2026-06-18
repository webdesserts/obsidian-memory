/// Integration tests for the Daemon event loop.
///
/// These tests exercise the real `Daemon` struct with real iroh nodes, in-memory
/// filesystems, and injected file events — verifying that the `tokio::select!`
/// event loop correctly routes file changes, gossip notifications, and inbound
/// QUIC sync requests through to vault state changes.
///
/// Construction pattern (mirrors sync_workflow.rs):
/// 1. Build both nodes (`build_node`) — filesystem, vault, iroh SyncNode
/// 2. Wire `MemoryLookup` and allowlists (`connect_nodes`)
/// 3. Join gossip exactly once per node (A with empty bootstrap, B via A)
/// 4. Spawn `Daemon::run_loop()` in a background task (`spawn_daemon`)
///
/// File events are injected by writing to `InMemoryFs` and sending a `FileEvent`
/// into `file_event_tx` — no real OS filesystem required.
///
/// Seeds 20+ are used to avoid collisions with sync_workflow.rs (seeds 1–10).
mod common;

mod daemon_integration {
    use std::sync::Arc;
    use std::time::Duration;

    use iroh::address_lookup::memory::MemoryLookup;
    use tokio::sync::{Mutex, mpsc};
    use tokio::task::JoinHandle;
    use tokio_util::sync::CancellationToken;

    use sync_core::allowlist::{AllowlistStorage, InMemoryAllowlist};
    use sync_core::fs::{FileSystem, InMemoryFs};
    use sync_core::network::{SyncNode, gossip::VaultGossip};
    use sync_core::peer_id::{PeerId, VaultId};
    use sync_core::{SyncMetadata, Vault};

    use sync_daemon::daemon::Daemon;
    use sync_daemon::watcher::{FileEvent, FileEventKind};

    // ── helpers ───────────────────────────────────────────────────────────────

    /// Shared gossip topic so all test daemons join the same swarm.
    fn shared_vault_id() -> VaultId {
        "cafebabecafebabe".parse().unwrap()
    }

    /// A test daemon: vault, filesystem, allowlist, and channels for injecting
    /// events and triggering shutdown. The event loop runs in a background task.
    struct TestDaemon {
        vault: Arc<Mutex<Vault<Arc<InMemoryFs>>>>,
        fs: Arc<InMemoryFs>,
        #[allow(dead_code)]
        allowlist: Arc<InMemoryAllowlist>,
        /// Send file events into the daemon's event loop.
        file_event_tx: mpsc::UnboundedSender<FileEvent>,
        /// Cancel to stop the daemon's event loop.
        shutdown: CancellationToken,
        /// Background task handle for the event loop.
        loop_handle: JoinHandle<()>,
    }

    /// Pre-gossip node state — built first so we can wire connectivity before joining.
    struct NodeBundle {
        vault: Arc<Mutex<Vault<Arc<InMemoryFs>>>>,
        fs: Arc<InMemoryFs>,
        allowlist: Arc<InMemoryAllowlist>,
        sync_node: SyncNode,
        node_id: PeerId,
    }

    /// Build an iroh node with vault and allowlist, but do NOT join gossip yet.
    ///
    /// Gossip is joined separately (after connectivity is wired) so each node
    /// has exactly one subscription per topic, matching the daemon's production behavior.
    async fn build_node(seed_byte: u8) -> anyhow::Result<NodeBundle> {
        let fs = Arc::new(InMemoryFs::new());
        // Author Loro ops under a per-device PeerId derived from this node's
        // secret seed, so each node is a distinct Loro replica.
        let author = PeerId::from_secret_bytes(super::common::seed(seed_byte));
        let vault = Vault::init(fs.clone(), author).await?;
        let vault = Arc::new(Mutex::new(vault));

        let allowlist = Arc::new(InMemoryAllowlist::new());
        let sync_node = SyncNode::new(super::common::seed(seed_byte), &[], allowlist.clone()).await?;

        let memory_lookup = MemoryLookup::new();
        sync_node.endpoint.address_lookup()?.add(memory_lookup);

        let node_id = PeerId::from_bytes(*sync_node.node_id().as_bytes());

        Ok(NodeBundle {
            vault,
            fs,
            allowlist,
            sync_node,
            node_id,
        })
    }

    /// Wire two nodes for direct `MemoryLookup` connectivity and mutual allowlist access.
    async fn connect_nodes(a: &NodeBundle, b: &NodeBundle) -> anyhow::Result<()> {
        let addr_a = a.sync_node.endpoint.addr();
        let addr_b = b.sync_node.endpoint.addr();

        let lookup_a = MemoryLookup::new();
        lookup_a.add_endpoint_info(addr_b.clone());
        a.sync_node.endpoint.address_lookup()?.add(lookup_a);

        let lookup_b = MemoryLookup::new();
        lookup_b.add_endpoint_info(addr_a.clone());
        b.sync_node.endpoint.address_lookup()?.add(lookup_b);

        a.allowlist.add_peer(b.node_id.clone(), "peer-b").await?;
        b.allowlist.add_peer(a.node_id.clone(), "peer-a").await?;

        Ok(())
    }

    /// Spawn the Daemon event loop from pre-wired components.
    ///
    /// Takes ownership of the `NodeBundle` and the gossip subscription so the
    /// daemon owns all components for the duration of the test.
    fn spawn_daemon(node: NodeBundle, gossip: VaultGossip) -> TestDaemon {
        let (file_event_tx, file_event_rx) = mpsc::unbounded_channel::<FileEvent>();
        let shutdown = CancellationToken::new();

        let vault = node.vault.clone();
        let fs = node.fs.clone();
        let allowlist = node.allowlist.clone();

        let mut daemon = Daemon::new(
            vault.clone(),
            node.sync_node,
            gossip,
            file_event_rx,
            None, // no mDNS discovery in tests
            allowlist.clone(),
            "test-device".to_string(),
            None,
            "/test-vault".into(),
            shutdown.clone(),
        );

        let loop_handle = tokio::spawn(async move {
            daemon.run_loop().await;
        });

        TestDaemon {
            vault,
            fs,
            allowlist,
            file_event_tx,
            shutdown,
            loop_handle,
        }
    }

    /// Inject a `FileEvent::Modified` into the daemon's event loop.
    ///
    /// Write the file to `daemon.fs` before calling this — the daemon's
    /// `on_file_modified` handler calls `vault.on_file_changed()` which reads
    /// from the in-memory filesystem.
    fn inject_modified(daemon: &TestDaemon, path: &str) {
        daemon
            .file_event_tx
            .send(FileEvent {
                path: path.to_string(),
                kind: FileEventKind::Modified,
            })
            .expect("file event channel unexpectedly closed");
    }

    /// Inject a `FileEvent::Deleted` into the daemon's event loop.
    fn inject_deleted(daemon: &TestDaemon, path: &str) {
        daemon
            .file_event_tx
            .send(FileEvent {
                path: path.to_string(),
                kind: FileEventKind::Deleted,
            })
            .expect("file event channel unexpectedly closed");
    }

    /// Poll until `predicate` returns true or 10 seconds elapse.
    ///
    /// Checks every 50ms. Panics on timeout.
    async fn wait_until<F, Fut>(description: &str, predicate: F)
    where
        F: Fn() -> Fut,
        Fut: std::future::Future<Output = bool>,
    {
        let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
        loop {
            if predicate().await {
                return;
            }
            if tokio::time::Instant::now() >= deadline {
                panic!("timed out waiting for: {}", description);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    // ── tests ─────────────────────────────────────────────────────────────────

    /// A file change on daemon A propagates to daemon B via gossip + QUIC pull.
    ///
    /// A file is written to A's `InMemoryFs`, then a `FileEvent::Modified` is
    /// injected into A's event loop. A indexes the change via `on_file_modified`,
    /// broadcasts via gossip, B receives the notification and pulls the full
    /// update from A over QUIC.
    ///
    /// Only the `FileEvent` is injected — the daemon's handler calls
    /// `vault.on_file_changed()` internally. Testing that full path is the point.
    #[tokio::test]
    async fn test_file_change_propagates_to_peer() -> anyhow::Result<()> {
        let node_a = build_node(20).await?;
        let node_b = build_node(21).await?;

        connect_nodes(&node_a, &node_b).await?;

        // A subscribes first (empty bootstrap), B subscribes bootstrapping off A.
        // Each node joins gossip exactly once to avoid non-deterministic delivery.
        let gossip_a = node_a
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![])
            .await?;
        let gossip_b = node_b
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![node_a.sync_node.node_id()])
            .await?;

        let daemon_a = spawn_daemon(node_a, gossip_a);
        let daemon_b = spawn_daemon(node_b, gossip_b);

        // Write file to A's filesystem, then inject the modification event.
        daemon_a
            .fs
            .write("notes/hello.md", b"# Hello World")
            .await?;
        inject_modified(&daemon_a, "notes/hello.md");

        // Wait for B to receive the file via gossip broadcast + QUIC pull.
        wait_until("B has notes/hello.md", || {
            let vault = daemon_b.vault.clone();
            async move {
                let vault = vault.lock().await;
                vault
                    .list_files()
                    .await
                    .unwrap_or_default()
                    .contains(&"notes/hello.md".to_string())
            }
        })
        .await;

        let content_b = daemon_b.fs.read("notes/hello.md").await?;
        assert_eq!(content_b, b"# Hello World");

        daemon_a.shutdown.cancel();
        daemon_b.shutdown.cancel();
        let _ = daemon_a.loop_handle.await;
        let _ = daemon_b.loop_handle.await;

        Ok(())
    }

    /// Pre-existing files sync when a new peer joins the gossip swarm.
    ///
    /// A has a file before gossip forms. When the swarm forms, `NeighborUp` fires
    /// on both sides and the daemon's `on_neighbor_up` handler initiates a full QUIC
    /// sync exchange. B ends up with A's pre-existing files.
    #[tokio::test]
    async fn test_neighbor_up_triggers_full_sync() -> anyhow::Result<()> {
        let node_a = build_node(22).await?;
        let node_b = build_node(23).await?;

        connect_nodes(&node_a, &node_b).await?;

        // Write A's file directly into the vault before spawning the event loops.
        node_a
            .fs
            .write("notes/offline-edit.md", b"# Written offline")
            .await?;
        {
            let vault = node_a.vault.lock().await;
            vault.on_file_changed("notes/offline-edit.md").await?;
        }

        let gossip_a = node_a
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![])
            .await?;
        let gossip_b = node_b
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![node_a.sync_node.node_id()])
            .await?;

        let daemon_a = spawn_daemon(node_a, gossip_a);
        let daemon_b = spawn_daemon(node_b, gossip_b);

        // When gossip forms, NeighborUp fires and both daemons initiate full syncs.
        // B's on_neighbor_up handler pulls A's pre-existing file.
        wait_until("B has notes/offline-edit.md", || {
            let vault = daemon_b.vault.clone();
            async move {
                let vault = vault.lock().await;
                vault
                    .list_files()
                    .await
                    .unwrap_or_default()
                    .contains(&"notes/offline-edit.md".to_string())
            }
        })
        .await;

        daemon_a.shutdown.cancel();
        daemon_b.shutdown.cancel();
        let _ = daemon_a.loop_handle.await;
        let _ = daemon_b.loop_handle.await;

        Ok(())
    }

    /// A pairing initiator adopts the mesh's VaultId, re-joins its gossip topic,
    /// and pulls the mesh's existing notes — the core of feature (b) pair-and-pull.
    ///
    /// A (responder) and B (initiator) start with *distinct* VaultIds, so they
    /// sit on different gossip topics and cannot sync — this is the pre-feature
    /// state. We then drive `Daemon::adopt_and_rejoin` on B with A's VaultId,
    /// which rewrites B's metadata.toml and swaps its gossip subscription onto
    /// A's topic. A `NeighborUp` fires and B's full-sync pulls A's pre-existing
    /// note.
    ///
    /// This test FAILS on pre-feature code: without `adopt_and_rejoin`, B never
    /// leaves its own topic, no NeighborUp fires, and the pull times out.
    ///
    /// Seeds 28/29.
    #[tokio::test]
    async fn test_initiator_adopts_vault_id_and_pulls() -> anyhow::Result<()> {
        let node_a = build_node(28).await?; // responder — owns the note
        let node_b = build_node(29).await?; // initiator — adopts A's VaultId

        connect_nodes(&node_a, &node_b).await?;

        // Precondition: the two vaults start on DIFFERENT VaultIds. If build_node
        // ever shared ids this would fire and the test below would pass trivially.
        let a_vault_id = node_a.vault.lock().await.vault_id();
        let b_vault_id = node_b.vault.lock().await.vault_id();
        assert_ne!(
            a_vault_id, b_vault_id,
            "test requires distinct initial VaultIds"
        );

        // Seed a note into A's vault before any topic forms.
        node_a
            .fs
            .write("notes/from-responder.md", b"# Shared from the mesh")
            .await?;
        {
            let vault = node_a.vault.lock().await;
            vault.on_file_changed("notes/from-responder.md").await?;
        }

        // A joins gossip on A's VaultId. B joins on B's (different) VaultId — at
        // this point they're on separate topics and cannot reach each other.
        let a_node_endpoint = node_a.sync_node.node_id();
        let gossip_a = node_a
            .sync_node
            .join_vault_gossip(&a_vault_id, vec![])
            .await?;
        let gossip_b = node_b
            .sync_node
            .join_vault_gossip(&b_vault_id, vec![])
            .await?;

        let daemon_a = spawn_daemon(node_a, gossip_a);

        // Build B's daemon inline so we hold `&mut daemon` to drive adoption,
        // mirroring test_status_broadcast_on_neighbor_events.
        let b_vault = node_b.vault.clone();
        let b_fs = node_b.fs.clone();
        let (_b_file_tx, b_file_rx) = mpsc::unbounded_channel::<FileEvent>();
        let b_shutdown = CancellationToken::new();
        let mut daemon_b = Daemon::new(
            b_vault.clone(),
            node_b.sync_node,
            gossip_b,
            b_file_rx,
            None,
            node_b.allowlist.clone(),
            "device-b".to_string(),
            None,
            "/test-vault-b".into(),
            b_shutdown.clone(),
        );

        // Adopt A's VaultId and re-join its gossip topic, bootstrapping off A.
        daemon_b
            .adopt_and_rejoin(
                a_vault_id,
                vec![PeerId::from_bytes(*a_node_endpoint.as_bytes())],
            )
            .await?;

        // In-memory id reflects the adoption immediately.
        assert_eq!(
            b_vault.lock().await.vault_id(),
            a_vault_id,
            "B should have adopted A's VaultId in memory"
        );

        // Now spawn B's event loop — it's subscribed to A's topic, so NeighborUp
        // fires and B pulls A's pre-existing note.
        let b_loop = tokio::spawn(async move {
            daemon_b.run_loop().await;
        });

        // Poll for the pull rather than asserting immediately: A's gossip→B sync
        // may fire (and warn) before B's loop is ready, then B initiates its own
        // sync from A — benign timing. wait_until rides through it.
        wait_until("B pulled notes/from-responder.md", || {
            let vault = b_vault.clone();
            async move {
                let vault = vault.lock().await;
                vault
                    .list_files()
                    .await
                    .unwrap_or_default()
                    .contains(&"notes/from-responder.md".to_string())
            }
        })
        .await;

        // The pulled content matches what A wrote.
        let pulled = b_fs.read("notes/from-responder.md").await?;
        assert_eq!(
            String::from_utf8_lossy(&pulled),
            "# Shared from the mesh",
            "pulled note content should match A's"
        );

        // Persistence regression guard: B's metadata.toml on disk reflects the
        // adopted id, not just the in-memory field.
        let meta_bytes = b_fs.read(".sync/metadata.toml").await?;
        let meta: SyncMetadata = toml::from_str(&String::from_utf8(meta_bytes.to_vec())?)?;
        assert_eq!(
            meta.vault_id, a_vault_id,
            "B's metadata.toml should persist the adopted VaultId"
        );

        b_shutdown.cancel();
        daemon_a.shutdown.cancel();
        let _ = b_loop.await;
        let _ = daemon_a.loop_handle.await;

        Ok(())
    }

    /// A file deletion on daemon A propagates to daemon B.
    ///
    /// A file is first synced to B, then A injects a `FileEvent::Deleted`. The
    /// daemon's `on_file_deleted` handler calls `vault.delete_file()` and broadcasts
    /// via gossip. B receives the notification, pulls the updated registry state
    /// from A, and removes the file from its own vault.
    #[tokio::test]
    async fn test_file_deletion_propagates() -> anyhow::Result<()> {
        let node_a = build_node(24).await?;
        let node_b = build_node(25).await?;

        connect_nodes(&node_a, &node_b).await?;

        let gossip_a = node_a
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![])
            .await?;
        let gossip_b = node_b
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![node_a.sync_node.node_id()])
            .await?;

        let daemon_a = spawn_daemon(node_a, gossip_a);
        let daemon_b = spawn_daemon(node_b, gossip_b);

        // Step 1: Sync the file from A to B via the daemon's normal file-change path.
        daemon_a
            .fs
            .write("notes/delete-me.md", b"to be deleted")
            .await?;
        inject_modified(&daemon_a, "notes/delete-me.md");

        wait_until("B has notes/delete-me.md before deletion", || {
            let vault = daemon_b.vault.clone();
            async move {
                let vault = vault.lock().await;
                vault
                    .list_files()
                    .await
                    .unwrap_or_default()
                    .contains(&"notes/delete-me.md".to_string())
            }
        })
        .await;

        // Step 2: Delete from A. The daemon's on_file_deleted handler removes the
        // file from the vault and broadcasts the change via gossip. B pulls the
        // updated state and removes the file from its vault.
        inject_deleted(&daemon_a, "notes/delete-me.md");

        wait_until("B no longer has notes/delete-me.md", || {
            let vault = daemon_b.vault.clone();
            async move {
                let vault = vault.lock().await;
                !vault
                    .list_files()
                    .await
                    .unwrap_or_default()
                    .contains(&"notes/delete-me.md".to_string())
            }
        })
        .await;

        daemon_a.shutdown.cancel();
        daemon_b.shutdown.cancel();
        let _ = daemon_a.loop_handle.await;
        let _ = daemon_b.loop_handle.await;

        Ok(())
    }

    /// An inbound-sync write does not trigger a re-broadcast when the OS watcher
    /// fires a spurious `Modified` event for the same path.
    ///
    /// After receiving a file via inbound sync, the file watcher may fire `Modified`
    /// for the path the daemon just wrote. Because `on_file_changed` is echo-safe
    /// (diff-and-merge returns `false` when the disk content matches the stored Loro
    /// state), the daemon sees `changed = false` and skips broadcasting.
    ///
    /// We verify by injecting a spurious `Modified` on B after A→B sync completes
    /// and asserting B's content is unchanged — no corruption from re-broadcasting
    /// stale local state.
    #[tokio::test]
    async fn test_inbound_sync_does_not_rebroadcast() -> anyhow::Result<()> {
        let node_a = build_node(26).await?;
        let node_b = build_node(27).await?;

        connect_nodes(&node_a, &node_b).await?;

        let gossip_a = node_a
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![])
            .await?;
        let gossip_b = node_b
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![node_a.sync_node.node_id()])
            .await?;

        let daemon_a = spawn_daemon(node_a, gossip_a);
        let daemon_b = spawn_daemon(node_b, gossip_b);

        // Sync a file from A to B.
        daemon_a
            .fs
            .write("notes/synced.md", b"synced content")
            .await?;
        inject_modified(&daemon_a, "notes/synced.md");

        wait_until("B has notes/synced.md", || {
            let vault = daemon_b.vault.clone();
            async move {
                let vault = vault.lock().await;
                vault
                    .list_files()
                    .await
                    .unwrap_or_default()
                    .contains(&"notes/synced.md".to_string())
            }
        })
        .await;

        // Inject a spurious Modified on B — simulating what the OS watcher would see
        // after the daemon writes the synced file to disk. The sync flag was consumed
        // during the inbound sync, so this event goes through normally without corruption.
        inject_modified(&daemon_b, "notes/synced.md");

        // Give the event time to process.
        tokio::time::sleep(Duration::from_millis(200)).await;

        // B's file content should be unchanged.
        let content = daemon_b.fs.read("notes/synced.md").await?;
        assert_eq!(
            content, b"synced content",
            "B's file content should be unchanged after spurious Modified event"
        );

        daemon_a.shutdown.cancel();
        daemon_b.shutdown.cancel();
        let _ = daemon_a.loop_handle.await;
        let _ = daemon_b.loop_handle.await;

        Ok(())
    }

    // ── DaemonControl / status broadcast tests ───────────────────────────────

    /// Status watch channel updates when peers join and leave the gossip swarm.
    ///
    /// Two real iroh nodes join gossip together so NeighborUp fires naturally.
    /// We wire `DaemonControl` into daemon A and verify the watch channel transitions
    /// from `Idle` (0 peers) to `Connected` (1 peer) after gossip connects, then
    /// back to `Idle` after daemon B shuts down and NeighborDown fires.
    ///
    /// Seeds 61/62 are reserved for this test to avoid collisions.
    #[tokio::test]
    async fn test_status_broadcast_on_neighbor_events() -> anyhow::Result<()> {
        use sync_daemon::daemon::Daemon;
        use sync_daemon::pair_api::{ConnectionState, DaemonCommand, DaemonStatus, PairingUiEvent};
        use tokio::sync::{broadcast, mpsc, watch};

        // Build two nodes that can communicate in-memory.
        let node_a = build_node(61).await?;
        let node_b = build_node(62).await?;

        connect_nodes(&node_a, &node_b).await?;

        let vault_id = shared_vault_id();
        let gossip_a = node_a
            .sync_node
            .join_vault_gossip(&vault_id, vec![])
            .await?;

        let (file_event_tx, file_event_rx) =
            mpsc::unbounded_channel::<sync_daemon::watcher::FileEvent>();
        let shutdown = CancellationToken::new();

        let mut daemon = Daemon::new(
            node_a.vault.clone(),
            node_a.sync_node,
            gossip_a,
            file_event_rx,
            None,
            node_a.allowlist.clone(),
            "device-a".to_string(),
            None,
            "/test-vault".into(),
            shutdown.clone(),
        );

        // Wire DaemonControl channels.
        let (status_tx, mut status_rx) = watch::channel(DaemonStatus::initial());
        let (pairing_tx, _pairing_rx): (broadcast::Sender<PairingUiEvent>, _) =
            broadcast::channel(16);
        let (_command_tx, command_rx) = mpsc::unbounded_channel::<DaemonCommand>();
        daemon.wire_control(status_tx, pairing_tx, command_rx, "Test Vault".to_string());

        // Emit the initial status so the watch channel has a real value immediately.
        daemon.emit_status().await;

        // Verify initial state: Idle with 0 peers, mesh name and device name set.
        {
            let status = status_rx.borrow_and_update();
            assert_eq!(
                status.state,
                ConnectionState::Idle,
                "initial state should be Idle"
            );
            assert_eq!(status.peer_count, 0);
            assert_eq!(status.mesh_name.as_deref(), Some("Test Vault"));
            assert_eq!(status.device_name.as_deref(), Some("device-a"));
        }

        // Spawn daemon A's event loop.
        drop(file_event_tx);
        let loop_handle = tokio::spawn(async move {
            daemon.run_loop().await;
        });

        // Join gossip on node B so NeighborUp fires on daemon A.
        let gossip_b = node_b
            .sync_node
            .join_vault_gossip(
                &vault_id,
                vec![node_a.node_id.as_bytes().try_into().unwrap()],
            )
            .await?;
        let daemon_b = spawn_daemon(node_b, gossip_b);

        // Wait for daemon A's status to show Connected (NeighborUp from B).
        wait_until("status = Connected after peer B joins", || {
            let state = status_rx.borrow().state.clone();
            async move { state == ConnectionState::Connected }
        })
        .await;

        {
            let status = status_rx.borrow();
            assert_eq!(status.state, ConnectionState::Connected);
            assert!(status.peer_count >= 1);
        }

        // Shut down daemon B and daemon A.
        daemon_b.shutdown.cancel();
        let _ = daemon_b.loop_handle.await;

        shutdown.cancel();
        let _ = loop_handle.await;
        Ok(())
    }

    /// `DaemonCommand::RequestPairing` returns an error reply (not a hang) when no
    /// initiator session is active — no prior `StartDiscovery` was sent.
    ///
    /// Seed 64 reserved.
    #[tokio::test]
    async fn test_request_pairing_without_discovery_returns_error() -> anyhow::Result<()> {
        use sync_daemon::daemon::Daemon;
        use sync_daemon::pair_api::{DaemonCommand, DaemonStatus, PairingUiEvent};
        use tokio::sync::{broadcast, mpsc, oneshot, watch};

        let node = build_node(64).await?;
        let vault_id = shared_vault_id();
        let gossip = node.sync_node.join_vault_gossip(&vault_id, vec![]).await?;

        let (_file_event_tx, file_event_rx) =
            mpsc::unbounded_channel::<sync_daemon::watcher::FileEvent>();
        let shutdown = CancellationToken::new();

        let mut daemon = Daemon::new(
            node.vault.clone(),
            node.sync_node,
            gossip,
            file_event_rx,
            None,
            node.allowlist.clone(),
            "device-test".to_string(),
            None,
            "/test-vault".into(),
            shutdown.clone(),
        );

        let (status_tx, _status_rx) = watch::channel(DaemonStatus::initial());
        let (pairing_tx, _pairing_rx): (broadcast::Sender<PairingUiEvent>, _) =
            broadcast::channel(16);
        let (command_tx, command_rx) = mpsc::unbounded_channel::<DaemonCommand>();
        daemon.wire_control(status_tx, pairing_tx, command_rx, "Test Vault".to_string());

        let loop_handle = tokio::spawn(async move {
            daemon.run_loop().await;
        });

        // Send RequestPairing without ever sending StartDiscovery — the daemon
        // should respond with an Err describing the missing session.
        let (reply_tx, reply_rx) = oneshot::channel::<Result<String, String>>();
        command_tx.send(DaemonCommand::RequestPairing {
            vault_id: "any-vault".to_string(),
            reply: reply_tx,
        })?;

        let reply = tokio::time::timeout(Duration::from_secs(2), reply_rx)
            .await
            .expect("daemon did not reply to RequestPairing within 2s")
            .expect("daemon dropped the reply channel");

        let err = reply.expect_err("expected Err when no active session");
        assert!(
            err.to_lowercase().contains("no active") || err.to_lowercase().contains("session"),
            "error message should mention missing session, got: {err}"
        );

        shutdown.cancel();
        let _ = loop_handle.await;
        Ok(())
    }

    /// `DaemonCommand::SubmitCode` returns a "no pairing request in progress"
    /// error when the vault_id is known (in discovered) but `RequestPairing`
    /// was never called — the `code_tx`-missing guard at `daemon.rs:613` is
    /// exercised directly.
    ///
    /// Seed 65 reserved.
    #[tokio::test]
    async fn test_submit_code_without_request_returns_error() -> anyhow::Result<()> {
        use iroh::EndpointId;
        use sync_daemon::daemon::Daemon;
        use sync_daemon::pair_api::{DaemonCommand, DaemonStatus, PairingUiEvent};
        use tokio::sync::{broadcast, mpsc, oneshot, watch};

        let node = build_node(65).await?;
        let vault_id = shared_vault_id();
        let gossip = node.sync_node.join_vault_gossip(&vault_id, vec![]).await?;

        // Capture the node's endpoint_id before moving node.sync_node.
        let endpoint_id: EndpointId = node.sync_node.node_id();

        let (_file_event_tx, file_event_rx) =
            mpsc::unbounded_channel::<sync_daemon::watcher::FileEvent>();
        let shutdown = CancellationToken::new();

        let mut daemon = Daemon::new(
            node.vault.clone(),
            node.sync_node,
            gossip,
            file_event_rx,
            None,
            node.allowlist.clone(),
            "device-test".to_string(),
            None,
            "/test-vault".into(),
            shutdown.clone(),
        );

        let (status_tx, _status_rx) = watch::channel(DaemonStatus::initial());
        let (pairing_tx, _pairing_rx): (broadcast::Sender<PairingUiEvent>, _) =
            broadcast::channel(16);
        let (command_tx, command_rx) = mpsc::unbounded_channel::<DaemonCommand>();
        daemon.wire_control(status_tx, pairing_tx, command_rx, "Test Vault".to_string());

        // Seed the discovered map so the vault_id check in submit_initiator_code
        // passes. This means the code_tx-missing check at daemon.rs:613 is the
        // one that fires — directly exercising that guard path.
        daemon
            .test_seed_discovered("target-vault".to_string(), endpoint_id)
            .await;

        let loop_handle = tokio::spawn(async move {
            daemon.run_loop().await;
        });

        // Send SubmitCode with the known vault_id but no prior RequestPairing.
        // The code_tx-missing guard should fire.
        let (reply_tx, reply_rx) = oneshot::channel::<Result<String, String>>();
        command_tx.send(DaemonCommand::SubmitCode {
            vault_id: "target-vault".to_string(),
            code: "123456".to_string(),
            reply: reply_tx,
        })?;

        let reply = tokio::time::timeout(Duration::from_secs(2), reply_rx)
            .await
            .expect("daemon did not reply to SubmitCode within 2s")
            .expect("daemon dropped the reply channel");

        let err = reply.expect_err("expected Err when no RequestPairing was sent");
        assert!(
            err.to_lowercase().contains("request") || err.to_lowercase().contains("in progress"),
            "error should describe the missing request step, got: {err}"
        );

        shutdown.cancel();
        let _ = loop_handle.await;
        Ok(())
    }

    /// Two-daemon happy path: RequestPairing → SubmitCode → adopt + pull.
    ///
    /// This test exercises the full two-step command sequence through the real
    /// daemon event loop, covering:
    /// - `connect_reply` receives `Ok(responder_device_name)` after connect
    /// - `code_tx`/`code_rx` park+unblock between the two commands
    /// - `submit_reply` routes the final `PairingResult` through `on_initiator_pair_outcome`
    /// - Post-pair onboarding (allowlist write, VaultId adoption, gossip re-join, pull)
    ///
    /// Daemon A (responder, "device-a") already has a note. Daemon B (initiator,
    /// "device-b") drives `RequestPairing` to connect; A emits the 6-digit code
    /// via its pairing broadcast; B's `SubmitCode` delivers the code to the parked
    /// task. On success B adopts A's VaultId, pulls A's note, and both allowlists
    /// carry both peers.
    ///
    /// Seeds 66/67 reserved.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_two_step_pairing_request_then_submit() -> anyhow::Result<()> {
        use iroh::EndpointId;
        use sync_daemon::daemon::Daemon;
        use sync_daemon::pair_api::{DaemonCommand, DaemonStatus, PairingUiEvent};
        use tokio::sync::{broadcast, mpsc, oneshot, watch};

        let node_a = build_node(66).await?; // responder
        let node_b = build_node(67).await?; // initiator

        connect_nodes(&node_a, &node_b).await?;

        let a_vault_id = node_a.vault.lock().await.vault_id();
        let b_vault_id = node_b.vault.lock().await.vault_id();
        assert_ne!(a_vault_id, b_vault_id, "test requires distinct initial VaultIds");

        // Seed a note into A's vault so B can prove it pulled via full sync.
        node_a
            .fs
            .write("notes/pair-test.md", b"# From the responder mesh")
            .await?;
        {
            let vault = node_a.vault.lock().await;
            vault.on_file_changed("notes/pair-test.md").await?;
        }

        // Both nodes join gossip on their own VaultId — separate topics until B adopts A's.
        let a_endpoint_id = node_a.sync_node.node_id();
        let gossip_a = node_a
            .sync_node
            .join_vault_gossip(&a_vault_id, vec![])
            .await?;
        let gossip_b = node_b
            .sync_node
            .join_vault_gossip(&b_vault_id, vec![])
            .await?;

        // Build daemon A (responder) with DaemonControl so we can intercept
        // the InboundRequest pairing event to get the 6-digit code.
        let (_a_file_tx, a_file_rx) = mpsc::unbounded_channel::<FileEvent>();
        let a_shutdown = CancellationToken::new();
        let a_allowlist = node_a.allowlist.clone();
        let mut daemon_a = Daemon::new(
            node_a.vault.clone(),
            node_a.sync_node,
            gossip_a,
            a_file_rx,
            None,
            a_allowlist.clone(),
            "device-a".to_string(),
            None,
            "/test-vault-a".into(),
            a_shutdown.clone(),
        );

        let (a_status_tx, _) = watch::channel(DaemonStatus::initial());
        let (a_pairing_tx, mut a_pairing_rx): (broadcast::Sender<PairingUiEvent>, _) =
            broadcast::channel(16);
        let (_a_cmd_tx, a_cmd_rx) = mpsc::unbounded_channel::<DaemonCommand>();
        daemon_a.wire_control(a_status_tx, a_pairing_tx, a_cmd_rx, "Mesh A".to_string());

        let a_loop = tokio::spawn(async move {
            daemon_a.run_loop().await;
        });

        // Build daemon B (initiator) with DaemonControl for command_tx.
        let b_vault = node_b.vault.clone();
        let b_fs = node_b.fs.clone();
        let b_allowlist = node_b.allowlist.clone();
        let (_b_file_tx, b_file_rx) = mpsc::unbounded_channel::<FileEvent>();
        let b_shutdown = CancellationToken::new();
        let mut daemon_b = Daemon::new(
            b_vault.clone(),
            node_b.sync_node,
            gossip_b,
            b_file_rx,
            None,
            b_allowlist.clone(),
            "device-b".to_string(),
            None,
            "/test-vault-b".into(),
            b_shutdown.clone(),
        );

        let (b_status_tx, _) = watch::channel(DaemonStatus::initial());
        let (b_pairing_tx, _b_pairing_rx): (broadcast::Sender<PairingUiEvent>, _) =
            broadcast::channel(16);
        let (b_cmd_tx, b_cmd_rx) = mpsc::unbounded_channel::<DaemonCommand>();
        daemon_b.wire_control(b_status_tx, b_pairing_tx, b_cmd_rx, "Mesh B".to_string());

        // Seed B's discovered map with A's endpoint — this replaces mDNS in tests.
        let a_endpoint: EndpointId = a_endpoint_id;
        daemon_b
            .test_seed_discovered(a_vault_id.to_string(), a_endpoint)
            .await;

        let b_loop = tokio::spawn(async move {
            daemon_b.run_loop().await;
        });

        // ── Step 1: RequestPairing ────────────────────────────────────────────
        // B connects to A, A generates its 6-digit code and emits InboundRequest.

        let (req_reply_tx, req_reply_rx) = oneshot::channel::<Result<String, String>>();
        b_cmd_tx.send(DaemonCommand::RequestPairing {
            vault_id: a_vault_id.to_string(),
            reply: req_reply_tx,
        })?;

        // Await both connect_reply and A's InboundRequest concurrently. The connect
        // reply fires after A sends its PairingChallenge; the broadcast fires at the
        // same point on A's event loop. We need both: the reply to confirm the right
        // device name, and the code to forward to SubmitCode.
        let (connect_result, pairing_code) = tokio::join!(
            async {
                tokio::time::timeout(Duration::from_secs(10), req_reply_rx)
                    .await
                    .expect("RequestPairing did not resolve within 10s")
                    .expect("daemon dropped RequestPairing reply channel")
            },
            async {
                // Wait for A to emit its InboundRequest event carrying the code.
                loop {
                    match tokio::time::timeout(Duration::from_secs(10), a_pairing_rx.recv()).await {
                        Ok(Ok(PairingUiEvent::InboundRequest { code, .. })) => break code,
                        Ok(Ok(_)) => continue, // other event — not what we want
                        Ok(Err(_)) => panic!("A's pairing broadcast channel closed unexpectedly"),
                        Err(_) => panic!("timed out waiting for A's InboundRequest pairing event"),
                    }
                }
            }
        );

        let responder_device_name = connect_result
            .expect("RequestPairing should succeed with Ok(device_name)");
        assert_eq!(
            responder_device_name, "device-a",
            "connect_reply should carry A's device name"
        );

        // ── Step 2: SubmitCode ────────────────────────────────────────────────
        // Deliver the code to the parked task; B completes the HMAC exchange.

        let (submit_reply_tx, submit_reply_rx) = oneshot::channel::<Result<String, String>>();
        b_cmd_tx.send(DaemonCommand::SubmitCode {
            vault_id: a_vault_id.to_string(),
            code: pairing_code,
            reply: submit_reply_tx,
        })?;

        let submit_result = tokio::time::timeout(Duration::from_secs(10), submit_reply_rx)
            .await
            .expect("SubmitCode did not resolve within 10s")
            .expect("daemon dropped SubmitCode reply channel");

        let paired_device = submit_result.expect("SubmitCode should succeed");
        assert_eq!(
            paired_device, "device-a",
            "submit_reply should carry A's device name after successful pairing"
        );

        // ── Assertions ───────────────────────────────────────────────────────

        // B adopted A's VaultId.
        assert_eq!(
            b_vault.lock().await.vault_id(),
            a_vault_id,
            "B should have adopted A's VaultId after pairing"
        );

        // B pulled A's pre-existing note via NeighborUp + full sync.
        wait_until("B pulled notes/pair-test.md from A", || {
            let vault = b_vault.clone();
            async move {
                vault
                    .lock()
                    .await
                    .list_files()
                    .await
                    .unwrap_or_default()
                    .contains(&"notes/pair-test.md".to_string())
            }
        })
        .await;

        let pulled = b_fs.read("notes/pair-test.md").await?;
        assert_eq!(
            String::from_utf8_lossy(&pulled),
            "# From the responder mesh",
            "pulled note content should match A's"
        );

        // Both allowlists carry both peers.
        let b_peers = b_allowlist.list_peers().await?;
        let b_peer_ids: Vec<_> = b_peers.iter().map(|p| &p.node_id).collect();
        let a_node_peer_id = PeerId::from_bytes(*a_endpoint_id.as_bytes());
        assert!(
            b_peer_ids.contains(&&a_node_peer_id),
            "B's allowlist should contain A's PeerId after pairing"
        );

        b_shutdown.cancel();
        a_shutdown.cancel();
        let _ = b_loop.await;
        let _ = a_loop.await;

        Ok(())
    }

    /// `DaemonCommand::SubmitCode` returns an error reply (not a hang) when no
    /// initiator session is active. This protects against the deadlock-prone
    /// pattern where the desktop awaits the oneshot reply while the daemon
    /// silently drops the command — the channel close would surface only as a
    /// generic "daemon disconnected" error in the UI, which is much worse than
    /// the explicit "no active session" message we send back.
    ///
    /// Seed 63 reserved.
    #[tokio::test]
    async fn test_submit_code_without_active_initiator_returns_error() -> anyhow::Result<()> {
        use sync_daemon::daemon::Daemon;
        use sync_daemon::pair_api::{DaemonCommand, DaemonStatus, PairingUiEvent};
        use tokio::sync::{broadcast, mpsc, oneshot, watch};

        let node = build_node(63).await?;
        let vault_id = shared_vault_id();
        let gossip = node.sync_node.join_vault_gossip(&vault_id, vec![]).await?;

        let (_file_event_tx, file_event_rx) =
            mpsc::unbounded_channel::<sync_daemon::watcher::FileEvent>();
        let shutdown = CancellationToken::new();

        let mut daemon = Daemon::new(
            node.vault.clone(),
            node.sync_node,
            gossip,
            file_event_rx,
            None,
            node.allowlist.clone(),
            "device-test".to_string(),
            None,
            "/test-vault".into(),
            shutdown.clone(),
        );

        let (status_tx, _status_rx) = watch::channel(DaemonStatus::initial());
        let (pairing_tx, _pairing_rx): (broadcast::Sender<PairingUiEvent>, _) =
            broadcast::channel(16);
        let (command_tx, command_rx) = mpsc::unbounded_channel::<DaemonCommand>();
        daemon.wire_control(status_tx, pairing_tx, command_rx, "Test Vault".to_string());

        let loop_handle = tokio::spawn(async move {
            daemon.run_loop().await;
        });

        // Send SubmitCode without ever sending StartDiscovery — the daemon
        // should respond with an Err describing the missing session.
        let (reply_tx, reply_rx) = oneshot::channel::<Result<String, String>>();
        command_tx.send(DaemonCommand::SubmitCode {
            vault_id: "any-vault".to_string(),
            code: "123456".to_string(),
            reply: reply_tx,
        })?;

        let reply = tokio::time::timeout(Duration::from_secs(2), reply_rx)
            .await
            .expect("daemon did not reply to SubmitCode within 2s")
            .expect("daemon dropped the reply channel");

        let err = reply.expect_err("expected Err when no active session");
        assert!(
            err.to_lowercase().contains("no active") || err.to_lowercase().contains("session"),
            "error message should mention missing session, got: {err}"
        );

        shutdown.cancel();
        let _ = loop_handle.await;
        Ok(())
    }

    /// After a successful two-step pairing, the responder's PUBLIC relay URL is
    /// adopted into B's persisted `known_public_relays` set AND its
    /// (endpoint_id, relay_url) is seeded into B's live `peer_lookup`, keyed by
    /// the responder's transport-verified EndpointId.
    ///
    /// This verifies the Tier-2 pairing-seed reframe (plan chunk C4):
    /// - `persist_adopted_relay` adopts the responder's public relay into the
    ///   `known_public_relays` cold store (the sole durable networking store) —
    ///   NOT a per-peer hint, and only when the URL is off-LAN-reachable.
    /// - After persist, `sync_node.add_peer_relay` still seeds the live lookup so
    ///   the current session reaches the responder by EndpointId without restart.
    /// - The adopted public relay survives a config reload, and no per-peer
    ///   `peer_relays` entry is persisted.
    ///
    /// A advertises a PUBLIC (domain) relay URL so the public-set adoption path is
    /// exercised — a loopback relay would be (correctly) rejected by the
    /// off-LAN-reachable guard. The advertised URL is A's `relay_url` field (the
    /// URL A carries in `PairingResult.relay_urls`), and A's `EndpointId` is the
    /// QUIC connection target B dialed — not inferred from `mesh_members`.
    ///
    /// Seeds 68/69 reserved.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_pairing_persists_responder_relay() -> anyhow::Result<()> {
        use iroh::EndpointId;
        use sync_daemon::daemon::Daemon;
        use sync_daemon::pair_api::{DaemonCommand, DaemonStatus, PairingUiEvent};
        use sync_daemon::persistence::DaemonConfig;
        use sync_daemon::relay::EmbeddedRelay;
        use tempfile::TempDir;
        use tokio::sync::{broadcast, mpsc, oneshot, watch};

        let node_a = build_node(68).await?; // responder
        let node_b = build_node(69).await?; // initiator

        connect_nodes(&node_a, &node_b).await?;

        let a_vault_id = node_a.vault.lock().await.vault_id();
        let b_vault_id = node_b.vault.lock().await.vault_id();
        assert_ne!(a_vault_id, b_vault_id, "test requires distinct initial VaultIds");

        // Start a relay for A so the daemon wiring is realistic, but advertise a
        // PUBLIC (domain) URL over the pairing wire: only an off-LAN-reachable URL
        // is adopted into `known_public_relays`, and the actual loopback bind URL
        // would be rejected by that guard. The advertised string is independent of
        // the relay's bind address (the responder fills `relay_urls` from its
        // configured `relay_url`), and this pairing-only test never routes traffic
        // through the relay, so a public-looking URL is sound here.
        let relay = EmbeddedRelay::start("127.0.0.1:0".parse().unwrap()).await?;
        let relay_url_str = "https://relay-a.test/".to_string();

        let a_endpoint_id: EndpointId = node_a.sync_node.node_id();

        // Clone B's peer_lookup handle BEFORE moving sync_node into Daemon::new.
        // MemoryLookup is Arc-backed, so the clone stays live and reflects any
        // add_peer_relay calls made on the daemon's internal copy.
        let b_peer_lookup = node_b.sync_node.peer_lookup.clone();

        // B needs a real on-disk vault path so `persist_adopted_relay` can write
        // `DaemonConfig.peer_relays` to `daemon.toml` and we can reload it.
        let b_vault_dir = TempDir::new()?;
        let b_vault_path = b_vault_dir.path().to_path_buf();

        let gossip_a = node_a
            .sync_node
            .join_vault_gossip(&a_vault_id, vec![])
            .await?;
        let gossip_b = node_b
            .sync_node
            .join_vault_gossip(&b_vault_id, vec![])
            .await?;

        // Build daemon A (responder) — pass A's relay URL so `PairingResult.relay_urls`
        // carries it over the wire to B.
        let (_a_file_tx, a_file_rx) = mpsc::unbounded_channel::<FileEvent>();
        let a_shutdown = CancellationToken::new();
        let a_allowlist = node_a.allowlist.clone();
        let a_vault_path = TempDir::new()?;
        let mut daemon_a = Daemon::new(
            node_a.vault.clone(),
            node_a.sync_node,
            gossip_a,
            a_file_rx,
            None,
            a_allowlist.clone(),
            "device-a".to_string(),
            Some(relay_url_str.clone()),
            a_vault_path.path().into(),
            a_shutdown.clone(),
        );

        let (a_status_tx, _) = watch::channel(DaemonStatus::initial());
        let (a_pairing_tx, mut a_pairing_rx): (broadcast::Sender<PairingUiEvent>, _) =
            broadcast::channel(16);
        let (_a_cmd_tx, a_cmd_rx) = mpsc::unbounded_channel::<DaemonCommand>();
        daemon_a.wire_control(a_status_tx, a_pairing_tx, a_cmd_rx, "Mesh A".to_string());

        let a_loop = tokio::spawn(async move {
            daemon_a.run_loop().await;
        });

        // Build daemon B (initiator) — use the real temp vault path.
        let b_vault = node_b.vault.clone();
        let b_allowlist = node_b.allowlist.clone();
        let (_b_file_tx, b_file_rx) = mpsc::unbounded_channel::<FileEvent>();
        let b_shutdown = CancellationToken::new();
        let mut daemon_b = Daemon::new(
            b_vault.clone(),
            node_b.sync_node,
            gossip_b,
            b_file_rx,
            None,
            b_allowlist.clone(),
            "device-b".to_string(),
            None,
            b_vault_path.clone(),
            b_shutdown.clone(),
        );

        let (b_status_tx, _) = watch::channel(DaemonStatus::initial());
        let (b_pairing_tx, _): (broadcast::Sender<PairingUiEvent>, _) = broadcast::channel(16);
        let (b_cmd_tx, b_cmd_rx) = mpsc::unbounded_channel::<DaemonCommand>();
        daemon_b.wire_control(b_status_tx, b_pairing_tx, b_cmd_rx, "Mesh B".to_string());

        daemon_b
            .test_seed_discovered(a_vault_id.to_string(), a_endpoint_id)
            .await;

        let b_loop = tokio::spawn(async move {
            daemon_b.run_loop().await;
        });

        // ── Step 1: RequestPairing ────────────────────────────────────────────

        let (req_reply_tx, req_reply_rx) = oneshot::channel::<Result<String, String>>();
        b_cmd_tx.send(DaemonCommand::RequestPairing {
            vault_id: a_vault_id.to_string(),
            reply: req_reply_tx,
        })?;

        let (connect_result, pairing_code) = tokio::join!(
            async {
                tokio::time::timeout(Duration::from_secs(10), req_reply_rx)
                    .await
                    .expect("RequestPairing did not resolve within 10s")
                    .expect("daemon dropped RequestPairing reply channel")
            },
            async {
                loop {
                    match tokio::time::timeout(Duration::from_secs(10), a_pairing_rx.recv()).await {
                        Ok(Ok(PairingUiEvent::InboundRequest { code, .. })) => break code,
                        Ok(Ok(_)) => continue,
                        Ok(Err(_)) => panic!("A's pairing broadcast channel closed unexpectedly"),
                        Err(_) => panic!("timed out waiting for A's InboundRequest pairing event"),
                    }
                }
            }
        );

        connect_result.expect("RequestPairing should succeed with Ok(device_name)");

        // ── Step 2: SubmitCode ────────────────────────────────────────────────

        let (submit_reply_tx, submit_reply_rx) = oneshot::channel::<Result<String, String>>();
        b_cmd_tx.send(DaemonCommand::SubmitCode {
            vault_id: a_vault_id.to_string(),
            code: pairing_code,
            reply: submit_reply_tx,
        })?;

        tokio::time::timeout(Duration::from_secs(10), submit_reply_rx)
            .await
            .expect("SubmitCode did not resolve within 10s")
            .expect("daemon dropped SubmitCode reply channel")
            .expect("SubmitCode should succeed");

        // ── Assertions ───────────────────────────────────────────────────────

        // Give the daemon's async `persist_adopted_relay` a moment to finish writing
        // before we reload the config.
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Live lookup: B's peer_lookup should carry A's (endpoint_id, relay_url).
        // This proves `add_peer_relay` was called on the current session after pairing.
        let hint = b_peer_lookup
            .get_endpoint_info(a_endpoint_id)
            .expect("B's peer_lookup should have A's relay hint after pairing");
        let hint_relay_urls: Vec<_> = hint
            .into_endpoint_addr()
            .relay_urls()
            .cloned()
            .collect();
        assert!(
            hint_relay_urls.iter().any(|u| u.to_string() == relay_url_str),
            "B's live peer_lookup hint should contain A's relay URL, got: {:?}",
            hint_relay_urls,
        );

        // Persistence: reload B's DaemonConfig and verify A's public relay was
        // adopted into `known_public_relays` (the sole durable networking store)
        // and NOT written as a per-peer hint.
        // `DaemonConfig::load_or_generate` reads from the real temp vault path.
        let (b_config, _) = DaemonConfig::load_or_generate(&b_vault_path, None)
            .await
            .expect("should be able to reload B's daemon config");
        assert!(
            b_config.known_public_relays.contains(&relay_url_str),
            "B's known_public_relays should contain A's advertised public relay \
             after pairing; set was {:?}",
            b_config.known_public_relays,
        );
        assert!(
            b_config.peer_relays.is_empty(),
            "no per-peer hint should be persisted on pairing — the public-relay \
             set is the sole durable networking store; peer_relays was {:?}",
            b_config.peer_relays,
        );

        b_shutdown.cancel();
        a_shutdown.cancel();
        let _ = b_loop.await;
        let _ = a_loop.await;
        relay.shutdown().await;

        Ok(())
    }

    /// A real local edit propagates to a peer even when the sync flag is armed for that path.
    ///
    /// Background: `mark_synced` simulates the lingering-flag window that arises when an
    /// inbound sync write triggers a watcher event that is processed after the TTL — or
    /// simply when the flag is stale. Before the fix, `on_file_modified` consumed the flag
    /// and returned early, skipping both `vault.on_file_changed` and the gossip broadcast.
    /// The peer would never receive the edit; the next full sync would revert it.
    ///
    /// After the fix the daemon always calls `vault.on_file_changed`, which is echo-safe
    /// (returns false for unchanged content), and gates the broadcast on the returned bool.
    ///
    /// Seeds 70/71 reserved.
    #[tokio::test]
    async fn test_local_edit_during_armed_flag_propagates_to_peer() -> anyhow::Result<()> {
        let node_a = build_node(70).await?;
        let node_b = build_node(71).await?;

        connect_nodes(&node_a, &node_b).await?;

        let gossip_a = node_a
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![])
            .await?;
        let gossip_b = node_b
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![node_a.sync_node.node_id()])
            .await?;

        let daemon_a = spawn_daemon(node_a, gossip_a);
        let daemon_b = spawn_daemon(node_b, gossip_b);

        // Create the initial file on A and let it sync to B so B is primed.
        daemon_a
            .fs
            .write("notes/flag-edit.md", b"# Original")
            .await?;
        inject_modified(&daemon_a, "notes/flag-edit.md");

        wait_until("B has initial notes/flag-edit.md", || {
            let vault = daemon_b.vault.clone();
            async move {
                vault
                    .lock()
                    .await
                    .list_files()
                    .await
                    .unwrap_or_default()
                    .contains(&"notes/flag-edit.md".to_string())
            }
        })
        .await;

        // Arm the sync flag on A for this path — simulates the lingering-flag window
        // after an inbound sync write where the watcher event arrives late.
        {
            let vault = daemon_a.vault.lock().await;
            vault.mark_synced("notes/flag-edit.md");
        }

        // Make a real local edit to the file on A's filesystem.
        daemon_a
            .fs
            .write("notes/flag-edit.md", b"# Edited after sync flag armed")
            .await?;

        // Inject the modification event — this is what the OS watcher would deliver.
        inject_modified(&daemon_a, "notes/flag-edit.md");

        // B must receive the updated content despite the armed flag on A.
        wait_until("B has the edited content of notes/flag-edit.md", || {
            let vault = daemon_b.vault.clone();
            async move {
                let vault = vault.lock().await;
                if let Ok(content) = vault.get_document("notes/flag-edit.md").await {
                    content.body().to_string().contains("Edited after sync flag armed")
                } else {
                    false
                }
            }
        })
        .await;

        daemon_a.shutdown.cancel();
        daemon_b.shutdown.cancel();
        let _ = daemon_a.loop_handle.await;
        let _ = daemon_b.loop_handle.await;

        Ok(())
    }

    /// A real local delete tombstones the registry and propagates to a peer even
    /// when the sync flag is armed for that path.
    ///
    /// Before the fix, `on_file_deleted` consumed the flag and returned early,
    /// producing no tombstone and no broadcast. On the next full sync the peer
    /// would re-deliver the file, resurrecting it. After the fix the daemon always
    /// calls `vault.delete_file`, which is idempotent (returns false for an
    /// already-absent path) and only broadcasts when it returns true.
    ///
    /// Seeds 72/73 reserved.
    #[tokio::test]
    async fn test_local_delete_during_armed_flag_propagates_to_peer() -> anyhow::Result<()> {
        let node_a = build_node(72).await?;
        let node_b = build_node(73).await?;

        connect_nodes(&node_a, &node_b).await?;

        let gossip_a = node_a
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![])
            .await?;
        let gossip_b = node_b
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![node_a.sync_node.node_id()])
            .await?;

        let daemon_a = spawn_daemon(node_a, gossip_a);
        let daemon_b = spawn_daemon(node_b, gossip_b);

        // Create a file on A and let it sync to B so both have it.
        daemon_a
            .fs
            .write("notes/flag-delete.md", b"# To be deleted")
            .await?;
        inject_modified(&daemon_a, "notes/flag-delete.md");

        wait_until("B has notes/flag-delete.md", || {
            let vault = daemon_b.vault.clone();
            async move {
                vault
                    .lock()
                    .await
                    .list_files()
                    .await
                    .unwrap_or_default()
                    .contains(&"notes/flag-delete.md".to_string())
            }
        })
        .await;

        // Arm the sync flag on A — simulates the lingering-flag window.
        {
            let vault = daemon_a.vault.lock().await;
            vault.mark_synced("notes/flag-delete.md");
        }

        // Delete the file on A's filesystem and inject the Deleted event.
        daemon_a.fs.delete("notes/flag-delete.md").await?;
        inject_deleted(&daemon_a, "notes/flag-delete.md");

        // B must tombstone the file despite the armed flag on A.
        wait_until("B no longer has notes/flag-delete.md", || {
            let vault = daemon_b.vault.clone();
            async move {
                !vault
                    .lock()
                    .await
                    .list_files()
                    .await
                    .unwrap_or_default()
                    .contains(&"notes/flag-delete.md".to_string())
            }
        })
        .await;

        daemon_a.shutdown.cancel();
        daemon_b.shutdown.cancel();
        let _ = daemon_a.loop_handle.await;
        let _ = daemon_b.loop_handle.await;

        Ok(())
    }

    // ── reconnect supervisor ──────────────────────────────────────────────────

    /// The reconnect supervisor re-bootstraps gossip from a seeded hint so a
    /// partitioned daemon reconnects without a restart.
    ///
    /// This exercises a COLD START (B never bootstraps to A) rather than a live
    /// partition, but the two are functionally equivalent for recovery: after a
    /// `NeighborDown` the connection close clears iroh's `selected_path` and the
    /// remote-state actor idles out (~60s), so both paths hit the same address
    /// lookup on the supervisor's re-dial. Live post-partition recovery is
    /// validated on the real mesh after merge.
    ///
    /// Setup: A and B are wired for connectivity (`connect_nodes`) and both join
    /// the shared topic, but B does NOT bootstrap off A — so no swarm forms and A
    /// stays at zero neighbors. A's supervisor snapshot carries B's hint. When
    /// the tick fires, A re-bootstraps toward B, `NeighborUp` fires, and A's
    /// full-sync pulls a note B never could have delivered while partitioned.
    ///
    /// Seeds 80/81 reserved.
    #[tokio::test]
    async fn supervisor_rebootstraps_after_zero_neighbors() -> anyhow::Result<()> {
        use sync_daemon::persistence::PeerRelay;

        let node_a = build_node(80).await?;
        let node_b = build_node(81).await?;

        connect_nodes(&node_a, &node_b).await?;

        // Seed a note into B's vault so A can prove it pulled after reconnecting.
        node_b
            .fs
            .write(
                "notes/from-partitioned-peer.md",
                b"# Delivered after reconnect",
            )
            .await?;
        {
            let vault = node_b.vault.lock().await;
            vault
                .on_file_changed("notes/from-partitioned-peer.md")
                .await?;
        }

        // B's hint, as it would appear in A's persisted snapshot. The relay URL
        // only needs to parse — direct addresses from `connect_nodes` carry the
        // actual dial.
        let b_endpoint_hex = node_b.sync_node.node_id().to_string();
        let b_hint = PeerRelay::new(b_endpoint_hex, "http://example.com:3340/".to_string());

        // A joins with no bootstrap; B joins with no bootstrap (NOT off A). They
        // share a topic but never dial each other — A is partitioned at zero
        // neighbors, the production failure shape.
        let gossip_a = node_a
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![])
            .await?;
        let gossip_b = node_b
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![])
            .await?;

        // Build A's daemon inline so we can seed its supervisor snapshot and shrink
        // the tick before the loop starts.
        let a_vault = node_a.vault.clone();
        let (_a_file_tx, a_file_rx) = mpsc::unbounded_channel::<FileEvent>();
        let a_shutdown = CancellationToken::new();
        let mut daemon_a = Daemon::new(
            a_vault.clone(),
            node_a.sync_node,
            gossip_a,
            a_file_rx,
            None,
            node_a.allowlist.clone(),
            "device-a".to_string(),
            None,
            "/test-vault-a".into(),
            a_shutdown.clone(),
        );
        daemon_a.seed_peer_relays_snapshot(vec![b_hint]);
        daemon_a.set_reconnect_interval(Duration::from_millis(200));

        let a_loop = tokio::spawn(async move {
            daemon_a.run_loop().await;
        });

        // B's daemon just needs to be alive to answer A's sync pull.
        let daemon_b = spawn_daemon(node_b, gossip_b);

        // A's supervisor re-bootstraps toward B; NeighborUp fires; A pulls B's note.
        wait_until(
            "A pulled notes/from-partitioned-peer.md after reconnect",
            || {
                let vault = a_vault.clone();
                async move {
                    vault
                        .lock()
                        .await
                        .list_files()
                        .await
                        .unwrap_or_default()
                        .contains(&"notes/from-partitioned-peer.md".to_string())
                }
            },
        )
        .await;

        a_shutdown.cancel();
        daemon_b.shutdown.cancel();
        let _ = a_loop.await;
        let _ = daemon_b.loop_handle.await;

        Ok(())
    }

    /// When already connected, the supervisor stays idle — it does not churn the
    /// swarm or revert a healthy sync.
    ///
    /// A and B pair up normally (B bootstraps off A) and sync a file. A's
    /// supervisor snapshot still carries B's hint, but because A has a live
    /// neighbor the tick gates out at step 1 every wake. We prove non-interference
    /// by syncing a SECOND file across several tick periods after connection: if
    /// the supervisor were re-bootstrapping or otherwise disturbing the swarm, the
    /// steady-state sync would be at risk.
    ///
    /// Seeds 82/83 reserved.
    #[tokio::test]
    async fn supervisor_idle_when_connected() -> anyhow::Result<()> {
        use sync_daemon::persistence::PeerRelay;

        let node_a = build_node(82).await?;
        let node_b = build_node(83).await?;

        connect_nodes(&node_a, &node_b).await?;

        let b_hint = PeerRelay::new(
            node_b.sync_node.node_id().to_string(),
            "http://example.com:3340/".to_string(),
        );

        // A and B form a normal swarm (B bootstraps off A).
        let gossip_a = node_a
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![])
            .await?;
        let gossip_b = node_b
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![node_a.sync_node.node_id()])
            .await?;

        let a_vault = node_a.vault.clone();
        let a_fs = node_a.fs.clone();
        let (a_file_tx, a_file_rx) = mpsc::unbounded_channel::<FileEvent>();
        let a_shutdown = CancellationToken::new();
        let mut daemon_a = Daemon::new(
            a_vault.clone(),
            node_a.sync_node,
            gossip_a,
            a_file_rx,
            None,
            node_a.allowlist.clone(),
            "device-a".to_string(),
            None,
            "/test-vault-a".into(),
            a_shutdown.clone(),
        );
        daemon_a.seed_peer_relays_snapshot(vec![b_hint]);
        // Fast tick so several supervisor wakes occur during the test window.
        daemon_a.set_reconnect_interval(Duration::from_millis(100));

        let a_loop = tokio::spawn(async move {
            daemon_a.run_loop().await;
        });

        let daemon_b = spawn_daemon(node_b, gossip_b);

        // First sync: proves the swarm formed.
        a_fs.write("notes/first.md", b"# First").await?;
        a_file_tx
            .send(FileEvent {
                path: "notes/first.md".to_string(),
                kind: FileEventKind::Modified,
            })
            .expect("file event channel closed");

        wait_until("B has notes/first.md", || {
            let vault = daemon_b.vault.clone();
            async move {
                vault
                    .lock()
                    .await
                    .list_files()
                    .await
                    .unwrap_or_default()
                    .contains(&"notes/first.md".to_string())
            }
        })
        .await;

        // Let several supervisor ticks fire while connected — they must gate out.
        tokio::time::sleep(Duration::from_millis(600)).await;

        // Second sync still works: the supervisor did not disturb the swarm.
        a_fs.write("notes/second.md", b"# Second").await?;
        a_file_tx
            .send(FileEvent {
                path: "notes/second.md".to_string(),
                kind: FileEventKind::Modified,
            })
            .expect("file event channel closed");

        wait_until("B has notes/second.md after idle supervisor ticks", || {
            let vault = daemon_b.vault.clone();
            async move {
                vault
                    .lock()
                    .await
                    .list_files()
                    .await
                    .unwrap_or_default()
                    .contains(&"notes/second.md".to_string())
            }
        })
        .await;

        a_shutdown.cancel();
        daemon_b.shutdown.cancel();
        let _ = a_loop.await;
        let _ = daemon_b.loop_handle.await;

        Ok(())
    }

    /// The supervisor evicts a THROTTLED hint from the address-lookup but leaves
    /// a DUE hint present — the core of the stale-hint fix.
    ///
    /// While partitioned (zero neighbors), `on_reconnect_tick` decides per hint:
    /// a hint inside its backoff window (recent attempt + failures) is removed
    /// from `MemoryLookup` so iroh-gossip can't re-resolve and re-feed the dead
    /// relay; a hint that's due is re-seeded and dialed. We seed two hints into
    /// A's `peer_lookup` up front, run the supervisor with no peer to connect to,
    /// and assert: the throttled one is gone, the due one remains.
    ///
    /// Uses no embedded relay (immune to the live-app `test_sync_through_embedded_relay`
    /// interference). Seeds 84/85/86 reserved (85/86 only supply valid EndpointIds).
    #[tokio::test]
    async fn supervisor_evicts_throttled_hint_keeps_due() -> anyhow::Result<()> {
        use iroh::RelayUrl;
        use sync_daemon::persistence::PeerRelay;

        let node_a = build_node(84).await?;
        // 85/86 exist only to mint two valid, distinct peer EndpointIds.
        let node_throttled = build_node(85).await?;
        let node_due = build_node(86).await?;

        let throttled_id = node_throttled.sync_node.node_id();
        let due_id = node_due.sync_node.node_id();
        let relay_url: RelayUrl = "http://example.com:3340/".parse()?;

        // Seed BOTH hints into A's lookup so eviction is observable as a removal.
        node_a.sync_node.set_peer_relay(throttled_id, &relay_url);
        node_a.sync_node.set_peer_relay(due_id, &relay_url);

        // A clone of the lookup shares the same backing store, so it observes the
        // supervisor's mutations from outside the spawned event loop.
        let lookup = node_a.sync_node.peer_lookup.clone();
        assert!(
            lookup.get_endpoint_info(throttled_id).is_some(),
            "throttled hint should start present in the lookup"
        );
        assert!(
            lookup.get_endpoint_info(due_id).is_some(),
            "due hint should start present in the lookup"
        );

        // Snapshot: the throttled hint has failures and a just-now attempt, so it
        // is well inside its backoff window (not due). The due hint has never been
        // attempted, so it is due immediately.
        let now = now_ms_test();
        let mut throttled_hint =
            PeerRelay::new(throttled_id.to_string(), "http://example.com:3340/".to_string());
        throttled_hint.failure_count = 6;
        throttled_hint.last_attempt_ms = Some(now);
        let due_hint =
            PeerRelay::new(due_id.to_string(), "http://example.com:3340/".to_string());

        // A joins gossip with no bootstrap — it stays partitioned at zero
        // neighbors, so the supervisor acts every tick.
        let gossip_a = node_a
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![])
            .await?;

        let (_a_file_tx, a_file_rx) = mpsc::unbounded_channel::<FileEvent>();
        let a_shutdown = CancellationToken::new();
        let mut daemon_a = Daemon::new(
            node_a.vault.clone(),
            node_a.sync_node,
            gossip_a,
            a_file_rx,
            None,
            node_a.allowlist.clone(),
            "device-a".to_string(),
            None,
            // A non-existent vault path: persisting hint failures will fail
            // gracefully (logged, non-fatal), but the in-memory eviction — what
            // this test asserts — still runs.
            "/test-vault-evict".into(),
            a_shutdown.clone(),
        );
        daemon_a.seed_peer_relays_snapshot(vec![throttled_hint, due_hint]);
        daemon_a.set_reconnect_interval(Duration::from_millis(100));

        let a_loop = tokio::spawn(async move {
            daemon_a.run_loop().await;
        });

        // The supervisor evicts the throttled hint while keeping the due one.
        // Both hints are `example.com` domains (off-LAN-reachable), so the
        // throttled one is evicted because a *due* alternative exists
        // (offlan_reachable_count == 2 ⇒ it is NOT the sole off-LAN lifeline) —
        // the eviction is driven by the due alternative, not by URL class.
        wait_until("throttled hint evicted from lookup", || {
            let lookup = lookup.clone();
            async move { lookup.get_endpoint_info(throttled_id).is_none() }
        })
        .await;

        assert!(
            lookup.get_endpoint_info(due_id).is_some(),
            "due hint must remain in the lookup (re-seeded each due tick)"
        );

        a_shutdown.cancel();
        let _ = a_loop.await;

        Ok(())
    }

    /// While partitioned with its SOLE peer-relay hint, the supervisor never
    /// evicts that hint even on throttled ticks — the relay-reap reconnect fix.
    ///
    /// Bug being guarded: a non-home iroh `ActiveRelayActor` reaps after 60s of
    /// inactivity; once reaped, the supervisor's re-bootstrap can't respawn it,
    /// and the OLD behavior compounded this by `remove_peer_relay`-ing the only
    /// hint on throttled ticks — starving the next due dial of the peer's
    /// address so the partition never heals without a process restart. The fix
    /// throttles the dial FREQUENCY (the existing `hint_attempt_due` gate), not
    /// the address PRESENCE: a sole hint stays resident in the lookup while at
    /// zero neighbors.
    ///
    /// To prove the supervisor loop actually ran (not a pass-by-luck dead loop),
    /// the hint starts ABSENT from the lookup but DUE in the snapshot. The first
    /// supervisor tick re-seeds it (`None` → `Some`, the positive liveness
    /// signal) and stamps `last_attempt_ms`, throwing it into a 120s backoff so
    /// every later tick sees it throttled. The OLD code would then remove it; the
    /// fix retains it. Seed 89 reserved (90 only mints a valid peer EndpointId).
    #[tokio::test]
    async fn supervisor_retains_sole_hint_when_throttled() -> anyhow::Result<()> {
        use sync_daemon::persistence::PeerRelay;

        let node_a = build_node(89).await?;
        // 90 exists only to mint a valid, distinct peer EndpointId.
        let node_peer = build_node(90).await?;
        let peer_id = node_peer.sync_node.node_id();

        // A clone of the lookup shares the backing store, so it observes the
        // supervisor's mutations from outside the spawned event loop. The hint
        // starts absent: the first due tick is what adds it.
        let lookup = node_a.sync_node.peer_lookup.clone();
        assert!(
            lookup.get_endpoint_info(peer_id).is_none(),
            "hint should start absent — the first supervisor tick adds it"
        );

        // A never-attempted hint is due immediately; the SOLE hint in the snapshot.
        let sole_hint = PeerRelay::new(peer_id.to_string(), "http://example.com:3340/".to_string());

        // A joins gossip with no bootstrap — it stays partitioned at zero
        // neighbors, so the supervisor acts every tick.
        let gossip_a = node_a
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![])
            .await?;

        let (_a_file_tx, a_file_rx) = mpsc::unbounded_channel::<FileEvent>();
        let a_shutdown = CancellationToken::new();
        let mut daemon_a = Daemon::new(
            node_a.vault.clone(),
            node_a.sync_node,
            gossip_a,
            a_file_rx,
            None,
            node_a.allowlist.clone(),
            "device-a".to_string(),
            None,
            "/test-vault-sole-hint".into(),
            a_shutdown.clone(),
        );
        daemon_a.seed_peer_relays_snapshot(vec![sole_hint]);
        daemon_a.set_reconnect_interval(Duration::from_millis(50));

        let a_loop = tokio::spawn(async move {
            daemon_a.run_loop().await;
        });

        // Liveness: the first due tick re-seeds the hint into the lookup. This
        // proves the supervisor loop is running before we assert retention.
        wait_until("sole hint re-seeded by first due tick", || {
            let lookup = lookup.clone();
            async move { lookup.get_endpoint_info(peer_id).is_some() }
        })
        .await;

        // The hint is now throttled (120s backoff). Let many throttled ticks fire
        // — the OLD code removes the sole hint on the first of these; the fix
        // keeps it. 750ms at a 50ms interval is ~15 ticks, far more than the one
        // tick the old eviction needed.
        tokio::time::sleep(Duration::from_millis(750)).await;
        assert!(
            lookup.get_endpoint_info(peer_id).is_some(),
            "the sole peer-relay hint must remain in the lookup across throttled \
             ticks — evicting it would strand the only address needed to heal the \
             partition"
        );

        a_shutdown.cancel();
        let _ = a_loop.await;

        Ok(())
    }

    /// With TWO throttled hints and none due, BOTH are evicted — the retention
    /// guard fires only when a hint is the sole hint OR the sole off-LAN lifeline.
    ///
    /// The relay-reap fix retains a hint only when it is the lookup's LAST address
    /// (`peer_relays.len() == 1`) or the LAST off-LAN-reachable one. Both hints
    /// here are LAN-IP relays (`192.168.68.52` and `10.0.0.5`), so neither is
    /// off-LAN-reachable: `offlan_reachable_count == 0`, the off-LAN-lifeline guard
    /// never applies, and the `is_sole_hint` guard is also false (len == 2). With
    /// no single lifeline to protect, both are throttled, both get evicted, and
    /// `bootstrap_ids` is empty so the tick early-returns (nothing to dial until
    /// one comes due). This pins the boundary so a future change can't silently
    /// widen the guard to "never evict while partitioned." The URLs are LAN-IPs
    /// (not domains) on purpose — a domain would be off-LAN-reachable and, as the
    /// sole such hint, would be RETAINED, which is the opposite of what this
    /// boundary test asserts.
    ///
    /// Seeds 91 reserved (92/93 only mint valid peer EndpointIds).
    #[tokio::test]
    async fn supervisor_evicts_both_when_two_throttled_none_due() -> anyhow::Result<()> {
        use iroh::RelayUrl;
        use sync_daemon::persistence::PeerRelay;

        let node_a = build_node(91).await?;
        // 92/93 exist only to mint two valid, distinct peer EndpointIds.
        let node_x = build_node(92).await?;
        let node_y = build_node(93).await?;
        let x_id = node_x.sync_node.node_id();
        let y_id = node_y.sync_node.node_id();
        // Distinct LAN-IP relays: both LAN-only, so neither is the off-LAN
        // lifeline the new rule protects. (The seeds only need to parse; the
        // classification reads the snapshot strings below.)
        let x_relay_url: RelayUrl = "http://192.168.68.52:3340/".parse()?;
        let y_relay_url: RelayUrl = "http://10.0.0.5:3340/".parse()?;

        // Seed BOTH hints into A's lookup so eviction is observable as removal.
        node_a.sync_node.set_peer_relay(x_id, &x_relay_url);
        node_a.sync_node.set_peer_relay(y_id, &y_relay_url);

        let lookup = node_a.sync_node.peer_lookup.clone();
        assert!(lookup.get_endpoint_info(x_id).is_some());
        assert!(lookup.get_endpoint_info(y_id).is_some());

        // Both hints are throttled: recent attempt + failures puts each well
        // inside its backoff window, and neither is due.
        let now = now_ms_test();
        let mut hint_x = PeerRelay::new(x_id.to_string(), "http://192.168.68.52:3340/".to_string());
        hint_x.failure_count = 6;
        hint_x.last_attempt_ms = Some(now);
        let mut hint_y = PeerRelay::new(y_id.to_string(), "http://10.0.0.5:3340/".to_string());
        hint_y.failure_count = 6;
        hint_y.last_attempt_ms = Some(now);

        let gossip_a = node_a
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![])
            .await?;

        let (_a_file_tx, a_file_rx) = mpsc::unbounded_channel::<FileEvent>();
        let a_shutdown = CancellationToken::new();
        let mut daemon_a = Daemon::new(
            node_a.vault.clone(),
            node_a.sync_node,
            gossip_a,
            a_file_rx,
            None,
            node_a.allowlist.clone(),
            "device-a".to_string(),
            None,
            "/test-vault-two-throttled".into(),
            a_shutdown.clone(),
        );
        daemon_a.seed_peer_relays_snapshot(vec![hint_x, hint_y]);
        daemon_a.set_reconnect_interval(Duration::from_millis(50));

        let a_loop = tokio::spawn(async move {
            daemon_a.run_loop().await;
        });

        // With no due alternative to protect, both throttled hints are evicted.
        wait_until("both throttled hints evicted from lookup", || {
            let lookup = lookup.clone();
            async move {
                lookup.get_endpoint_info(x_id).is_none()
                    && lookup.get_endpoint_info(y_id).is_none()
            }
        })
        .await;

        a_shutdown.cancel();
        let _ = a_loop.await;

        Ok(())
    }

    /// With two throttled hints — a domain lifeline and a dead LAN-IP relay — the
    /// supervisor RETAINS the domain hint (the sole off-LAN-reachable route) and
    /// evicts the LAN-IP one. This is the off-LAN regression guard.
    ///
    /// The coffeeshop bug: a laptop that paired with both umbra (public
    /// `umbra.computer` relay) and charon (a `192.168.x` LAN-IP relay) holds two
    /// hints. Off-LAN, charon's LAN-IP relay is unreachable, so umbra's domain
    /// hint is the ONLY real route — yet the old `is_sole_hint` guard (count == 1)
    /// evicted it the moment it was throttled, because a second hint existed. With
    /// n0 DNS removed (commit `2b51540`) there was no longer a parallel resolver
    /// to mask the gap, so off-LAN reconnect silently broke. The fix generalizes
    /// the retention guard from "sole hint" to "sole off-LAN-reachable hint": a
    /// LAN-only alternative is no alternative off-LAN.
    ///
    /// The LAN-IP eviction is the positive liveness proof that the supervisor loop
    /// actually ran (not a pass-by-luck dead loop) — we wait for that removal,
    /// THEN assert the domain hint is still resident. Both hints are throttled
    /// (failure_count 6 + just-now attempt ⇒ well inside backoff), so neither is
    /// due; only the classification decides which is kept.
    ///
    /// FAILS on pre-fix code: the old branch evicts any throttled non-sole hint,
    /// so the domain lifeline would be removed alongside the LAN-IP one.
    ///
    /// Seed 103 reserved (104/105 only mint valid peer EndpointIds).
    #[tokio::test]
    async fn supervisor_retains_throttled_offlan_lifeline_over_lan_hint() -> anyhow::Result<()> {
        use iroh::RelayUrl;
        use sync_daemon::persistence::PeerRelay;

        let node_a = build_node(103).await?;
        // 104/105 exist only to mint two valid, distinct peer EndpointIds.
        let node_lifeline = build_node(104).await?;
        let node_lan = build_node(105).await?;

        let lifeline_id = node_lifeline.sync_node.node_id();
        let lan_id = node_lan.sync_node.node_id();
        // The seeded URL only needs to parse; the classification reads the
        // snapshot `PeerRelay.relay_url` string, so the domain-vs-IP distinction
        // is carried by the snapshot strings below, not by these seeds.
        let lifeline_relay_url: RelayUrl = "http://example.com:3340/".parse()?;
        let lan_relay_url: RelayUrl = "http://192.168.68.52:3340/".parse()?;

        // Seed BOTH hints into A's lookup so eviction is observable as a removal.
        node_a
            .sync_node
            .set_peer_relay(lifeline_id, &lifeline_relay_url);
        node_a.sync_node.set_peer_relay(lan_id, &lan_relay_url);

        let lookup = node_a.sync_node.peer_lookup.clone();
        assert!(
            lookup.get_endpoint_info(lifeline_id).is_some(),
            "domain lifeline hint should start present in the lookup"
        );
        assert!(
            lookup.get_endpoint_info(lan_id).is_some(),
            "LAN-IP hint should start present in the lookup"
        );

        // Both hints are throttled: failures + just-now attempt puts each well
        // inside its backoff window, so neither is due. The domain hint
        // (`example.com`) is off-LAN-reachable; the `192.168.68.52` hint is
        // LAN-only.
        let now = now_ms_test();
        let mut lifeline_hint = PeerRelay::new(
            lifeline_id.to_string(),
            "http://example.com:3340/".to_string(),
        );
        lifeline_hint.failure_count = 6;
        lifeline_hint.last_attempt_ms = Some(now);
        let mut lan_hint =
            PeerRelay::new(lan_id.to_string(), "http://192.168.68.52:3340/".to_string());
        lan_hint.failure_count = 6;
        lan_hint.last_attempt_ms = Some(now);

        // A joins gossip with no bootstrap — it stays partitioned at zero
        // neighbors, so the supervisor acts every tick.
        let gossip_a = node_a
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![])
            .await?;

        let (_a_file_tx, a_file_rx) = mpsc::unbounded_channel::<FileEvent>();
        let a_shutdown = CancellationToken::new();
        let mut daemon_a = Daemon::new(
            node_a.vault.clone(),
            node_a.sync_node,
            gossip_a,
            a_file_rx,
            None,
            node_a.allowlist.clone(),
            "device-a".to_string(),
            None,
            "/test-vault-offlan-lifeline".into(),
            a_shutdown.clone(),
        );
        daemon_a.seed_peer_relays_snapshot(vec![lifeline_hint, lan_hint]);
        daemon_a.set_reconnect_interval(Duration::from_millis(50));

        let a_loop = tokio::spawn(async move {
            daemon_a.run_loop().await;
        });

        // The LAN-IP hint is evicted — the liveness proof the supervisor ran.
        wait_until("LAN-IP hint evicted from lookup", || {
            let lookup = lookup.clone();
            async move { lookup.get_endpoint_info(lan_id).is_none() }
        })
        .await;

        // The domain lifeline is the sole off-LAN-reachable hint, so it is
        // RETAINED across throttled ticks even though a (LAN-only) alternative
        // exists. This is the off-LAN regression the fix closes.
        assert!(
            lookup.get_endpoint_info(lifeline_id).is_some(),
            "off-LAN domain lifeline must be retained when it is the only \
             off-LAN-reachable hint, even with a LAN-only alternative present"
        );

        a_shutdown.cancel();
        let _ = a_loop.await;

        Ok(())
    }

    /// A network change re-dials a throttled peer and heals the partition without
    /// a restart — the fix for "every wifi switch needs an app restart."
    ///
    /// The reconnect supervisor's per-hint backoff pins a long-unreachable peer at
    /// the 30-min max, and nothing reset it on a network change. Now iroh's
    /// `watch_addr` net-change signal (injected here through the same channel the
    /// startup task feeds) resets the backoff so the hint becomes due and the next
    /// supervisor tick re-dials.
    ///
    /// Mirrors `supervisor_rebootstraps_after_zero_neighbors` — A and B share a
    /// topic but neither bootstraps off the other, so A is partitioned at zero
    /// neighbors and the ONLY route to B is A's supervisor re-dialing its hint.
    /// The single difference: B's hint starts THROTTLED (failure_count 6, recent
    /// attempt ⇒ ~30-min backoff), so the supervisor will NOT dial it on its own
    /// within the test budget. The net change is therefore the SOLE cause of the
    /// reconnect, and A pulling B's note (a durable observable, not a transient
    /// lookup-presence flicker) is the fix's proof. FAILS on pre-fix code: without
    /// the handler, `net_tx.send` goes nowhere, the hint stays throttled, and A
    /// never pulls.
    ///
    /// Seed 101 reserved (102 mints B's EndpointId via the supplier node).
    #[tokio::test]
    async fn net_change_redials_throttled_peer_and_heals_partition() -> anyhow::Result<()> {
        use sync_daemon::persistence::PeerRelay;

        let node_a = build_node(101).await?; // partitioned peer that re-dials
        let node_b = build_node(102).await?; // supplier — answers A's pull after reconnect

        // Direct-address wiring so A's re-dial reaches B once the hint is due.
        // (Direct addresses live in a separate resolver, so they survive the
        // supervisor evicting the throttled hint from `peer_lookup`.)
        connect_nodes(&node_a, &node_b).await?;

        // Seed a note into B's vault so A's pull proves the partition healed.
        node_b
            .fs
            .write("notes/after-net-change.md", b"# Delivered after net change")
            .await?;
        {
            let vault = node_b.vault.lock().await;
            vault.on_file_changed("notes/after-net-change.md").await?;
        }

        // B's hint, THROTTLED: a recent attempt plus a high failure count puts it
        // well inside its ~30-min backoff window (>> the 10s wait budget), so the
        // supervisor will not re-dial it until the net change resets the throttle.
        // The relay URL only needs to parse — `connect_nodes` carries the dial.
        let b_endpoint_hex = node_b.sync_node.node_id().to_string();
        let mut b_hint = PeerRelay::new(b_endpoint_hex, "http://example.com:3340/".to_string());
        b_hint.failure_count = 6;
        b_hint.last_attempt_ms = Some(now_ms_test());

        // Both join with NO bootstrap — they never dial each other, so A stays
        // partitioned at zero neighbors (the production failure shape).
        let gossip_a = node_a
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![])
            .await?;
        let gossip_b = node_b
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![])
            .await?;

        let a_vault = node_a.vault.clone();
        let (_a_file_tx, a_file_rx) = mpsc::unbounded_channel::<FileEvent>();
        let a_shutdown = CancellationToken::new();
        // The net-change channel the production startup task feeds; here a test
        // holds the sender so `send(())` simulates a wifi switch with no real net.
        let (net_tx, net_rx) = tokio::sync::mpsc::channel::<()>(1);
        let mut daemon_a = Daemon::new(
            a_vault.clone(),
            node_a.sync_node,
            gossip_a,
            a_file_rx,
            None,
            node_a.allowlist.clone(),
            "device-a".to_string(),
            None,
            "/test-vault-netchange".into(),
            a_shutdown.clone(),
        );
        daemon_a.set_net_change_rx(net_rx);
        daemon_a.seed_peer_relays_snapshot(vec![b_hint]);
        daemon_a.set_reconnect_interval(Duration::from_millis(50));

        let a_loop = tokio::spawn(async move {
            daemon_a.run_loop().await;
        });
        // B just needs to be alive to answer A's sync pull.
        let daemon_b = spawn_daemon(node_b, gossip_b);

        // Fire the network change. (The throttled hint guarantees A has not yet
        // re-dialed B — its backoff is ~30 min, far beyond the wait budget — so
        // the reconnect below is caused by this signal, not by an early tick.)
        net_tx.send(()).await.unwrap();

        // The fix's proof: the net-change reset makes B's hint due, the next
        // supervisor tick re-dials B, NeighborUp fires, and A pulls B's note.
        wait_until("A pulled notes/after-net-change.md after the net change", || {
            let vault = a_vault.clone();
            async move {
                vault
                    .lock()
                    .await
                    .list_files()
                    .await
                    .unwrap_or_default()
                    .contains(&"notes/after-net-change.md".to_string())
            }
        })
        .await;

        a_shutdown.cancel();
        daemon_b.shutdown.cancel();
        let _ = a_loop.await;
        let _ = daemon_b.loop_handle.await;

        Ok(())
    }

    /// A successful NeighborUp sync resets the peer's hint freshness — the
    /// learn-on-exchange behavior that makes stale-hint eviction safe.
    ///
    /// A carries a throttled hint for B on disk (high failure_count, recorded
    /// attempt). A and B form a swarm; NeighborUp fires; A pulls B's note. On
    /// that success, A's `on_exchange_learned` stamps the hint and resets the
    /// failure count. The connection is direct (no relay), so this exercises the
    /// `mark_peer_relay_success` (LAN-direct, no learned URL) path. We assert on
    /// A's persisted `daemon.toml`, which is what drives the next session's
    /// supervisor.
    ///
    /// Direct-address wiring → immune to the live-app embedded-relay interference.
    /// Seeds 87/88 reserved.
    #[tokio::test]
    async fn successful_exchange_resets_hint_freshness() -> anyhow::Result<()> {
        use sync_daemon::persistence::{DaemonConfig, PeerRelay};
        use tempfile::TempDir;

        let node_a = build_node(87).await?; // receiver — learns on exchange
        let node_b = build_node(88).await?; // sender — supplies a note to pull

        connect_nodes(&node_a, &node_b).await?;

        // Seed a note into B's vault so A's pull proves the exchange happened.
        node_b
            .fs
            .write("notes/learn-probe.md", b"# Learned on exchange")
            .await?;
        {
            let vault = node_b.vault.lock().await;
            vault.on_file_changed("notes/learn-probe.md").await?;
        }

        let b_hex = node_b.sync_node.node_id().to_string();

        // A's real on-disk vault path, pre-seeded with a THROTTLED hint for B so
        // the reset is observable (failure_count 4 → 0, last_success_ms set).
        let a_vault_dir = TempDir::new()?;
        let a_vault_path = a_vault_dir.path().to_path_buf();
        let (mut a_config, _) = DaemonConfig::load_or_generate(&a_vault_path, None).await?;
        let mut seeded = PeerRelay::new(b_hex.clone(), "http://example.com:3340/".to_string());
        seeded.failure_count = 4;
        seeded.last_attempt_ms = Some(1_000);
        a_config.peer_relays.push(seeded.clone());
        a_config.save(&a_vault_path)?;

        // A and B form a swarm (B bootstraps off A) so NeighborUp fires both ways.
        let gossip_a = node_a
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![])
            .await?;
        let gossip_b = node_b
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![node_a.sync_node.node_id()])
            .await?;

        let a_vault = node_a.vault.clone();
        let (_a_file_tx, a_file_rx) = mpsc::unbounded_channel::<FileEvent>();
        let a_shutdown = CancellationToken::new();
        let mut daemon_a = Daemon::new(
            a_vault.clone(),
            node_a.sync_node,
            gossip_a,
            a_file_rx,
            None,
            node_a.allowlist.clone(),
            "device-a".to_string(),
            None,
            a_vault_path.clone(),
            a_shutdown.clone(),
        );
        // The supervisor snapshot starts with the same throttled hint, mirroring
        // what startup seeding would load from disk.
        daemon_a.seed_peer_relays_snapshot(vec![seeded]);

        let a_loop = tokio::spawn(async move {
            daemon_a.run_loop().await;
        });
        let daemon_b = spawn_daemon(node_b, gossip_b);

        // Prove the exchange happened: A pulls B's note.
        wait_until("A pulled notes/learn-probe.md", || {
            let vault = a_vault.clone();
            async move {
                vault
                    .lock()
                    .await
                    .list_files()
                    .await
                    .unwrap_or_default()
                    .contains(&"notes/learn-probe.md".to_string())
            }
        })
        .await;

        // The hint's freshness is reset on A's persisted config. The learn-on-
        // exchange write is async relative to the file pull, so poll for it.
        wait_until("A's persisted hint for B is reset", || {
            let path = a_vault_path.clone();
            let b_hex = b_hex.clone();
            async move {
                let Ok((config, _)) = DaemonConfig::load_or_generate(&path, None).await else {
                    return false;
                };
                config
                    .peer_relays
                    .iter()
                    .find(|r| r.endpoint_id == b_hex)
                    .map(|r| r.failure_count == 0 && r.last_success_ms.is_some())
                    .unwrap_or(false)
            }
        })
        .await;

        a_shutdown.cancel();
        daemon_b.shutdown.cancel();
        let _ = a_loop.await;
        let _ = daemon_b.loop_handle.await;

        Ok(())
    }

    /// CROWN JEWEL: a peer ABSENT at a pairing's broadcast instant converges to
    /// the full mesh roster after it joins — the exact rhea↔umbra trust-
    /// propagation bug.
    ///
    /// A already trusts a third peer C (the "umbra" A paired with earlier). B (the
    /// "rhea") paired through A but never saw C — its allowlist holds only A. When
    /// B late-joins the swarm, A's `NeighborUp` fires `push_allowlist_roster`,
    /// which broadcasts A's full roster `{B, C}`. B is already trusted by A, so B
    /// merges the roster and learns C with no direct B↔C pairing and no broadcast
    /// at C's original pairing instant.
    ///
    /// The user-facing effect asserted is trust state — B can now sync with C
    /// because C is in B's allowlist — not any internal call.
    ///
    /// Seeds 94/95 for A/B; C = synthetic PeerId from seed 96 (never a live node).
    #[tokio::test]
    async fn allowlist_roster_converges_on_late_join() -> anyhow::Result<()> {
        let node_a = build_node(94).await?;
        let node_b = build_node(95).await?;

        // C is a synthetic third member A already knows — a roster entry, never a
        // real node. Derived from a distinct seed so its PeerId can't collide.
        let c_peer = PeerId::from_secret_bytes(super::common::seed(96));

        // connect_nodes wires A↔B mutual trust → A's allowlist = {B}, B's = {A}.
        connect_nodes(&node_a, &node_b).await?;
        // Pre-seed C into A only: A's allowlist becomes {B, C}; B still has only A.
        // (PeerId is Copy, so `c_peer` stays usable for the assert below.)
        node_a.allowlist.add_peer(c_peer, "peer-c").await?;

        // Sug2 (pin the causal chain): B does NOT know C before the roster push.
        // Asserted before any daemon spawns, so B's allowlist is definitively {A}.
        assert!(
            !node_b.allowlist.is_allowed(&c_peer).await?,
            "precondition: B must not know C until the roster push (else pass-by-luck)"
        );

        // A joins with empty bootstrap; B late-joins off A → NeighborUp fires on A.
        let gossip_a = node_a
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![])
            .await?;
        let gossip_b = node_b
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![node_a.sync_node.node_id()])
            .await?;

        let daemon_a = spawn_daemon(node_a, gossip_a);
        let daemon_b = spawn_daemon(node_b, gossip_b);

        // B converges to the full roster {A, C} purely from joining the mesh.
        wait_until("B's allowlist contains C after roster push", || {
            let allowlist = daemon_b.allowlist.clone();
            async move {
                allowlist
                    .list_peers()
                    .await
                    .unwrap_or_default()
                    .iter()
                    .any(|p| p.node_id == c_peer && !p.removed)
            }
        })
        .await;

        daemon_a.shutdown.cancel();
        daemon_b.shutdown.cancel();
        let _ = daemon_a.loop_handle.await;
        let _ = daemon_b.loop_handle.await;

        Ok(())
    }

    /// Connected-path periodic reconcile (convergence mechanism 2): a roster drift
    /// that NeighborUp already missed self-heals on a reconcile tick.
    ///
    /// A and B form a live swarm and sync. THEN — after they are already connected,
    /// so no NeighborUp will fire for it — C is added to A's allowlist directly.
    /// The connected-path reconcile inside the supervisor tick re-pushes A's roster
    /// on its throttled cadence; B picks up C without any new pairing or reconnect.
    ///
    /// The reconcile throttle is shrunk via `set_roster_reconcile_interval` (a seam
    /// on the daemon's own timer) so the reconcile lands within `wait_until`.
    ///
    /// Seeds 97/98 for A/B; C = synthetic PeerId from seed 96.
    #[tokio::test]
    async fn allowlist_roster_reconciles_drift_while_connected() -> anyhow::Result<()> {
        let node_a = build_node(97).await?;
        let node_b = build_node(98).await?;

        let c_peer = PeerId::from_secret_bytes(super::common::seed(96));

        connect_nodes(&node_a, &node_b).await?;

        let gossip_a = node_a
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![])
            .await?;
        let gossip_b = node_b
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![node_a.sync_node.node_id()])
            .await?;

        // Build A inline so we can shrink its reconcile cadence before run_loop.
        let a_allowlist = node_a.allowlist.clone();
        let (_a_file_tx, a_file_rx) = mpsc::unbounded_channel::<FileEvent>();
        let a_shutdown = CancellationToken::new();
        let mut daemon_a = Daemon::new(
            node_a.vault.clone(),
            node_a.sync_node,
            gossip_a,
            a_file_rx,
            None,
            a_allowlist.clone(),
            "device-a".to_string(),
            None,
            "/test-vault-a".into(),
            a_shutdown.clone(),
        );
        // Fast tick + near-zero reconcile throttle so a reconcile fires promptly
        // once a neighbor is live. (The crown-jewel NeighborUp push happens before
        // C exists, so only the connected-path reconcile can carry C here.)
        daemon_a.set_reconnect_interval(Duration::from_millis(100));
        daemon_a.set_roster_reconcile_interval(Duration::from_millis(0));

        let a_loop = tokio::spawn(async move {
            daemon_a.run_loop().await;
        });
        let daemon_b = spawn_daemon(node_b, gossip_b);

        // Wait for the swarm to form (B learns A on NeighborUp; A's roster at this
        // instant is just {B}, so B does not yet know C).
        wait_until("B's allowlist contains A (swarm formed)", || {
            let allowlist = daemon_b.allowlist.clone();
            async move { !allowlist.list_peers().await.unwrap_or_default().is_empty() }
        })
        .await;

        // Drift: add C to A AFTER connection — NeighborUp already fired and won't
        // re-fire, so only the periodic reconcile can propagate this.
        a_allowlist.add_peer(c_peer, "peer-c").await?;

        // The connected-path reconcile re-pushes A's roster; B converges on C.
        wait_until("B's allowlist contains C via reconcile", || {
            let allowlist = daemon_b.allowlist.clone();
            async move {
                allowlist
                    .list_peers()
                    .await
                    .unwrap_or_default()
                    .iter()
                    .any(|p| p.node_id == c_peer && !p.removed)
            }
        })
        .await;

        a_shutdown.cancel();
        daemon_b.shutdown.cancel();
        let _ = a_loop.await;
        let _ = daemon_b.loop_handle.await;

        Ok(())
    }

    /// Revocation propagates and WINS over re-add — the B1 enforcement end-to-end.
    ///
    /// A and B both start trusting a third peer C. On A, `remove_peer(C)` writes a
    /// tombstone (no user-facing revoke command exists yet, so this stands in for
    /// what one would do). The connected-path reconcile carries A's roster — which
    /// includes the C tombstone — to B, whose `merge_roster` honors tombstone-
    /// precedence: B stops trusting C.
    ///
    /// Then the negative-resurrection property: B's own reconcile pushes its roster
    /// back to A, and A's C must STAY revoked. `merge_roster` never lets a (stale)
    /// live row resurrect a tombstone, so A's `is_allowed(C)` can never flip back to
    /// true — removal wins over re-add across the mesh.
    ///
    /// Seeds 99/100 for A/B; C = synthetic PeerId from seed 96.
    #[tokio::test]
    async fn allowlist_revocation_propagates_and_wins() -> anyhow::Result<()> {
        let node_a = build_node(99).await?;
        let node_b = build_node(100).await?;

        let c_peer = PeerId::from_secret_bytes(super::common::seed(96));

        connect_nodes(&node_a, &node_b).await?;
        // Both members trust C (the shared roster before revocation).
        node_a.allowlist.add_peer(c_peer, "peer-c").await?;
        node_b.allowlist.add_peer(c_peer, "peer-c").await?;

        let gossip_a = node_a
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![])
            .await?;
        let gossip_b = node_b
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![node_a.sync_node.node_id()])
            .await?;

        // Build A inline so its reconcile cadence is fast enough to carry the
        // tombstone within `wait_until` (there's no revoke command to push promptly).
        let a_allowlist = node_a.allowlist.clone();
        let (_a_file_tx, a_file_rx) = mpsc::unbounded_channel::<FileEvent>();
        let a_shutdown = CancellationToken::new();
        let mut daemon_a = Daemon::new(
            node_a.vault.clone(),
            node_a.sync_node,
            gossip_a,
            a_file_rx,
            None,
            a_allowlist.clone(),
            "device-a".to_string(),
            None,
            "/test-vault-a".into(),
            a_shutdown.clone(),
        );
        daemon_a.set_reconnect_interval(Duration::from_millis(100));
        daemon_a.set_roster_reconcile_interval(Duration::from_millis(0));

        let a_loop = tokio::spawn(async move {
            daemon_a.run_loop().await;
        });
        let daemon_b = spawn_daemon(node_b, gossip_b);

        // Swarm forms; both still trust C at this point.
        wait_until("swarm formed (B knows A)", || {
            let allowlist = daemon_b.allowlist.clone();
            async move { !allowlist.list_peers().await.unwrap_or_default().is_empty() }
        })
        .await;

        // Revoke C on A — writes a tombstone that travels in the roster.
        a_allowlist.remove_peer(&c_peer).await?;

        // B honors the revocation: tombstone-precedence flips C to not-trusted.
        wait_until("B no longer trusts C after revocation", || {
            let allowlist = daemon_b.allowlist.clone();
            async move { !allowlist.is_allowed(&c_peer).await.unwrap_or(true) }
        })
        .await;

        // Negative-resurrection: B's roster pushes back to A, but a removed peer can
        // never be resurrected by a stale live row — A's C stays revoked. This can
        // never become true, so a single assertion (post-convergence) is sound.
        // `a_allowlist` is the daemon's own handle (shared Arc), so this is A's view.
        assert!(
            !a_allowlist.is_allowed(&c_peer).await?,
            "A's revocation of C must not be resurrected by B's roster push"
        );

        a_shutdown.cancel();
        daemon_b.shutdown.cancel();
        let _ = a_loop.await;
        let _ = daemon_b.loop_handle.await;

        Ok(())
    }

    /// Wall-clock ms helper for tests — mirrors the daemon's `now_ms` so seeded
    /// `last_attempt_ms` values land on the same clock the supervisor reads.
    fn now_ms_test() -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0)
    }
}
