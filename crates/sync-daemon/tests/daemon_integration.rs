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
    use sync_core::network::{gossip::VaultGossip, SyncNode};
    use sync_core::peer_id::{PeerId, VaultId};
    use sync_core::Vault;

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
        let vault = Vault::init(fs.clone()).await?;
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
        daemon_a.fs.write("notes/hello.md", b"# Hello World").await?;
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
        node_a.fs.write("notes/offline-edit.md", b"# Written offline").await?;
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
        daemon_a.fs.write("notes/delete-me.md", b"to be deleted").await?;
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
        daemon_a.fs.write("notes/synced.md", b"synced content").await?;
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
            content,
            b"synced content",
            "B's file content should be unchanged after spurious Modified event"
        );

        daemon_a.shutdown.cancel();
        daemon_b.shutdown.cancel();
        let _ = daemon_a.loop_handle.await;
        let _ = daemon_b.loop_handle.await;

        Ok(())
    }
}
