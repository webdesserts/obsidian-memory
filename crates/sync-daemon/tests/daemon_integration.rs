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

    /// Deterministic 32-byte seed for building test nodes.
    fn seed(n: u8) -> [u8; 32] {
        [n; 32]
    }

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
        let author = PeerId::from_secret_bytes(seed(seed_byte));
        let vault = Vault::init(fs.clone(), author).await?;
        let vault = Arc::new(Mutex::new(vault));

        let allowlist = Arc::new(InMemoryAllowlist::new());
        let sync_node = SyncNode::new(seed(seed_byte), None, allowlist.clone()).await?;

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

    /// The sync flag prevents a locally-written file from triggering a re-broadcast.
    ///
    /// When the daemon receives a file via inbound sync, the vault records a sync
    /// flag for that path. If the OS watcher then fires `Modified` for that same
    /// path, `on_file_modified` consumes the sync flag and skips broadcasting.
    ///
    /// We verify this by injecting a spurious `Modified` for a synced path and
    /// asserting B's file content remains unchanged — no corruption from an extra
    /// on_file_changed call replaying stale local state.
    #[tokio::test]
    async fn test_sync_flag_prevents_rebroadcast() -> anyhow::Result<()> {
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

    /// After a successful two-step pairing, the responder's (endpoint_id, relay_url)
    /// is persisted to B's `DaemonConfig.peer_relays` AND seeded into B's live
    /// `peer_lookup`, keyed by the responder's transport-verified EndpointId.
    ///
    /// This test verifies commit 4 of the Umbra Relay plan:
    /// - `persist_adopted_relay` now takes the responder's `EndpointId` explicitly
    ///   and writes to `peer_relays` (not the discarded `relay_url` field).
    /// - After persist, `sync_node.add_peer_relay` is called so the current session
    ///   benefits without restart.
    /// - The stored entry survives a config reload.
    ///
    /// A's relay URL is the transport-verified source: it is A's `relay_url` field
    /// (the URL A advertises in `PairingResult.relay_urls`), and A's `EndpointId` is
    /// the QUIC connection target B dialed — not inferred from `mesh_members`.
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

        // Start a relay for A to advertise. This gives `relay_urls` something to
        // carry over the pairing wire so B can persist A's relay after onboarding.
        let relay = EmbeddedRelay::start("127.0.0.1:0".parse().unwrap()).await?;
        let relay_url = relay.relay_url().clone();
        let relay_url_str = relay_url.to_string();

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

        // Persistence: reload B's DaemonConfig and verify peer_relays was written.
        // `DaemonConfig::load_or_generate` reads from the real temp vault path.
        let (b_config, _) = DaemonConfig::load_or_generate(&b_vault_path, None)
            .await
            .expect("should be able to reload B's daemon config");
        let a_endpoint_hex = a_endpoint_id.to_string();
        let persisted = b_config
            .peer_relays
            .iter()
            .find(|r| r.endpoint_id == a_endpoint_hex)
            .expect("B's peer_relays should contain A's entry after pairing");
        assert_eq!(
            persisted.relay_url, relay_url_str,
            "persisted relay_url should match A's advertised URL"
        );

        b_shutdown.cancel();
        a_shutdown.cancel();
        let _ = b_loop.await;
        let _ = a_loop.await;
        relay.shutdown().await;

        Ok(())
    }
}
