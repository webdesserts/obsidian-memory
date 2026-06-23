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
    use sync_core::network::{SyncNode, gossip::VaultGossip};
    use sync_core::peer_id::{PeerId, VaultId};
    use uuid::Uuid;
    use vault_sync::fs::{FileSystem, InMemoryFs};
    use vault_sync::{ContentDoc, SyncMetadata, Vault, content_hash};

    use sync_daemon::daemon::Daemon;
    // Boot-recovery helpers shared with `startup_inner` (P4f-2b-ii) — the
    // `spawn_daemon_from_loaded` harness runs the SAME sequence as production.
    use sync_daemon::daemon::{clear_pending_journal, read_pending_journal, restitch_inputs};
    use sync_daemon::move_coalescer::{
        JournaledMove, PENDING_MOVES_VERSION, PendingKind, PendingMovesFile, hex_lower,
    };
    use sync_daemon::watcher::{FileEvent, FileEventKind};

    // ── helpers ───────────────────────────────────────────────────────────────

    /// Shared gossip topic so all test daemons join the same swarm.
    ///
    /// Returns a `sync_core::VaultId` — the iroh gossip layer (`join_vault_gossip`)
    /// speaks sync-core's VaultId. The vault's own `vault_id()` returns a
    /// `vault_sync::VaultId`; the two are bridged through `u64` at the gossip boundary.
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
        /// The inbound-sync freshness receiver paired with the pumped handler's
        /// sender. Wired into the daemon (`set_inbound_seen_rx`) by `spawn_daemon`
        /// so inbound-only peers are stamped alive (S2). Tests building a `Daemon`
        /// inline can take it too, or drop it (the handler's `send` then fails
        /// quietly — inert, since those tests don't assert inbound-only liveness).
        inbound_seen_rx: mpsc::UnboundedReceiver<PeerId>,
    }

    /// Build an iroh node with vault and allowlist, but do NOT join gossip yet.
    ///
    /// Gossip is joined separately (after connectivity is wired) so each node
    /// has exactly one subscription per topic, matching the daemon's production behavior.
    ///
    /// The node is built with the daemon's PUMPED inbound handler (via
    /// `new_with_sync_handler`), not the default one-shot `SyncStreamHandler` —
    /// the convergence tests exercise the multi-message vault-sync handshake, so
    /// the inbound side must pump exactly as production does.
    async fn build_node(seed_byte: u8) -> anyhow::Result<NodeBundle> {
        let fs = Arc::new(InMemoryFs::new());
        // Author Loro ops under a per-device PeerId derived from this node's
        // secret seed, so each node is a distinct Loro replica.
        let author = PeerId::from_secret_bytes(super::common::seed(seed_byte));
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
            super::common::seed(seed_byte),
            &[],
            allowlist.clone(),
            sync_handler,
        )
        .await?;

        let memory_lookup = MemoryLookup::new();
        sync_node.endpoint.address_lookup()?.add(memory_lookup);

        let node_id = PeerId::from_bytes(*sync_node.node_id().as_bytes());

        Ok(NodeBundle {
            vault,
            fs,
            allowlist,
            sync_node,
            node_id,
            inbound_seen_rx,
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
        let inbound_seen_rx = node.inbound_seen_rx;

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
        // Wire the pumped handler's freshness receiver so inbound-only peers are
        // stamped alive (S2) — mirrors `startup.rs`.
        daemon.set_inbound_seen_rx(inbound_seen_rx);
        // Wire the SAME in-memory fs the vault uses so the move-coalescer's
        // crash-recovery journal (`.sync/pending-moves.json`, P4f-2) lands in the
        // same stateful `InMemoryFs` as the vault's `.loro`/`.md`. `InMemoryFs` is
        // stateful, so this MUST be `node.fs`, not a fresh instance. The daemon's
        // `FS` is `Arc<InMemoryFs>`, so the handle is double-Arc'd — harmless, the
        // `FileSystem for Arc<T>` blanket impl makes it a usable filesystem.
        daemon.set_fs(Arc::new(fs.clone()));

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

    /// The daemon drives the variable-length vault-sync handshake to termination
    /// over a single QUIC bi-stream — the pumped exchange (X), not a one-shot
    /// request/reply/half-close.
    ///
    /// Two scenarios in one test (shared two-daemon setup):
    ///
    /// 1. **Diverged pair converges through the full pump.** A holds a note B
    ///    lacks, so the digests differ. B's `NeighborUp` opens a sync to A; the
    ///    exchange pumps `SyncRequest → DigestMismatch → SyncExchange →
    ///    SyncResponse`. We assert BOTH sides end byte-identical — not just that B
    ///    received A's note, but that the pumped responder converged in-exchange
    ///    too (A and B share the same `.md` set and the same content at each path).
    ///
    /// 2. **Converged pair settles in a no-op without corruption.** After step 1
    ///    the vaults are identical. A fresh NeighborUp (driven by a spurious
    ///    same-content edit on B) exchanges `SyncRequest → InSync` and transfers no
    ///    content — observed as: both vaults keep the identical note set with
    ///    byte-identical content, no spurious churn.
    ///
    /// The byte-level "zero content on a no-op" proof lives in vault-sync's
    /// `ByteCounter`/`full_sync_counting` at the lib layer; this daemon-level test
    /// proves the DAEMON drives the pump to a clean terminus over real QUIC.
    #[tokio::test]
    async fn test_daemon_pumps_variable_length_handshake() -> anyhow::Result<()> {
        let node_a = build_node(30).await?;
        let node_b = build_node(31).await?;

        connect_nodes(&node_a, &node_b).await?;

        // Scenario 1 — diverged: A has two notes B lacks (offline edits).
        for (path, body) in [
            ("notes/alpha.md", b"# Alpha".as_slice()),
            ("notes/beta.md", b"# Beta".as_slice()),
        ] {
            node_a.fs.write(path, body).await?;
            let vault = node_a.vault.lock().await;
            vault.on_file_changed(path).await?;
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

        // The pumped exchange must converge BOTH sides to byte-identical state:
        // same `.md` set + same content at every path. A bidirectional pump (each
        // side initiates on NeighborUp) settles the responder in-exchange too, so
        // we assert full convergence, not merely "B received A's notes".
        wait_until("A and B converge to byte-identical state", || {
            let vault_a = daemon_a.vault.clone();
            let vault_b = daemon_b.vault.clone();
            let fs_a = daemon_a.fs.clone();
            let fs_b = daemon_b.fs.clone();
            async move {
                let files_a = vault_a.lock().await.list_files().await.unwrap_or_default();
                let files_b = vault_b.lock().await.list_files().await.unwrap_or_default();
                let mut a_sorted = files_a.clone();
                let mut b_sorted = files_b.clone();
                a_sorted.sort();
                b_sorted.sort();
                if a_sorted != b_sorted
                    || !a_sorted.contains(&"notes/alpha.md".to_string())
                    || !a_sorted.contains(&"notes/beta.md".to_string())
                {
                    return false;
                }
                // Byte-identical content at every shared path.
                for path in &a_sorted {
                    let ca = fs_a.read(path).await.ok();
                    let cb = fs_b.read(path).await.ok();
                    if ca.is_none() || ca != cb {
                        return false;
                    }
                }
                true
            }
        })
        .await;

        // Snapshot the converged content so scenario 2 can prove the no-op pump
        // left it untouched.
        let alpha_before = daemon_b.fs.read("notes/alpha.md").await?;

        // Scenario 2 — converged no-op: a spurious same-content Modified on B.
        // vault-sync's diff-and-merge yields no change (content matches the stored
        // Loro state), so this re-exercises the digest fast-path on the next
        // exchange. The pumped `SyncRequest → InSync` transfers no content; we
        // observe that the converged state is preserved with no corruption.
        inject_modified(&daemon_b, "notes/alpha.md");

        // Let the spurious event and any resulting exchange settle.
        tokio::time::sleep(Duration::from_millis(300)).await;

        let alpha_after = daemon_b.fs.read("notes/alpha.md").await?;
        assert_eq!(
            alpha_before, alpha_after,
            "a converged-pair no-op exchange must not alter materialized content"
        );
        // A still has both notes unchanged — no spurious content churn from the no-op.
        let files_a = daemon_a
            .vault
            .lock()
            .await
            .list_files()
            .await
            .unwrap_or_default();
        assert!(
            files_a.contains(&"notes/alpha.md".to_string())
                && files_a.contains(&"notes/beta.md".to_string()),
            "A's note set must be intact after the no-op exchange"
        );

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
            .join_vault_gossip(&VaultId::from(a_vault_id.as_u64()), vec![])
            .await?;
        let gossip_b = node_b
            .sync_node
            .join_vault_gossip(&VaultId::from(b_vault_id.as_u64()), vec![])
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
        // `adopt_and_rejoin` speaks the iroh layer's `sync_core::VaultId`; bridge
        // A's vault-sync VaultId through `u64`.
        daemon_b
            .adopt_and_rejoin(
                VaultId::from(a_vault_id.as_u64()),
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
    ///
    /// The file is seeded via the NeighborUp full sync rather than a `Modified`
    /// event so the deletion's gossip `ChangeNotification{path}` is the FIRST
    /// notification for that path. A `Modified`-seeded create broadcasts an
    /// IDENTICAL `ChangeNotification{path}`, and iroh-gossip suppresses a
    /// byte-identical message within a 90s window by its content-derived id — so a
    /// create-notification followed by a same-path delete-notification collides and
    /// the deletion is dropped at the receiver. That fragility is pre-existing and
    /// orthogonal to coalescing (a manual ~500ms delay before a `Modified`-seeded
    /// create + delete reproduces it on the pre-coalescer engine); the move-coalescer's
    /// intended buffering delay (P4f-1) merely makes the collision window reliable
    /// rather than timing-lucky. Seeding via full sync isolates the delete-propagation
    /// contract this test owns from that notification-layer issue (Issue 2 / anti-entropy).
    #[tokio::test]
    async fn test_file_deletion_propagates() -> anyhow::Result<()> {
        let node_a = build_node(24).await?;
        let node_b = build_node(25).await?;

        connect_nodes(&node_a, &node_b).await?;

        // Seed the file into A's vault BEFORE gossip forms so B receives it via the
        // NeighborUp full sync (no create `ChangeNotification` broadcast).
        node_a
            .fs
            .write("notes/delete-me.md", b"to be deleted")
            .await?;
        {
            let vault = node_a.vault.lock().await;
            vault.on_file_changed("notes/delete-me.md").await?;
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
        assert_ne!(
            a_vault_id, b_vault_id,
            "test requires distinct initial VaultIds"
        );

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
            .join_vault_gossip(&VaultId::from(a_vault_id.as_u64()), vec![])
            .await?;
        let gossip_b = node_b
            .sync_node
            .join_vault_gossip(&VaultId::from(b_vault_id.as_u64()), vec![])
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

        let responder_device_name =
            connect_result.expect("RequestPairing should succeed with Ok(device_name)");
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
        assert_ne!(
            a_vault_id, b_vault_id,
            "test requires distinct initial VaultIds"
        );

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
        // `known_public_relays` to `daemon.toml` and we can reload it.
        let b_vault_dir = TempDir::new()?;
        let b_vault_path = b_vault_dir.path().to_path_buf();

        let gossip_a = node_a
            .sync_node
            .join_vault_gossip(&VaultId::from(a_vault_id.as_u64()), vec![])
            .await?;
        let gossip_b = node_b
            .sync_node
            .join_vault_gossip(&VaultId::from(b_vault_id.as_u64()), vec![])
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
        let hint_relay_urls: Vec<_> = hint.into_endpoint_addr().relay_urls().cloned().collect();
        assert!(
            hint_relay_urls
                .iter()
                .any(|u| u.to_string() == relay_url_str),
            "B's live peer_lookup hint should contain A's relay URL, got: {:?}",
            hint_relay_urls,
        );

        // Persistence: reload B's DaemonConfig and verify A's public relay was
        // adopted into `known_public_relays` — the sole durable networking store.
        // (No per-peer hint is persisted; the `peer_relays` field is gone.)
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

        b_shutdown.cancel();
        a_shutdown.cancel();
        let _ = b_loop.await;
        let _ = a_loop.await;
        relay.shutdown().await;

        Ok(())
    }

    /// A real local edit propagates to a peer.
    ///
    /// This guards the daemon's `on_file_modified` → `on_file_changed` (true) → gossip
    /// broadcast path end-to-end: an edit on A reaches B. Echo-safety is a content-diff in
    /// `on_file_changed` (it returns `false` only for unchanged content, suppressing the
    /// re-broadcast of an inbound-sync echo); the daemon no longer consults any path-keyed
    /// sync flag, so there is nothing to "arm" — a real edit always applies and broadcasts.
    ///
    /// The initial file is seeded via the NeighborUp full sync (not a `Modified` event) so
    /// the EDIT's gossip `ChangeNotification{path}` is the FIRST notification for that path.
    /// A `Modified`-seeded create broadcasts an IDENTICAL `ChangeNotification{path}`, and
    /// iroh-gossip's content-id dedup drops a same-path follow-up within its 90s window once
    /// the create's broadcast is delayed — so an edit shortly after a create would be
    /// suppressed at the receiver. That fragility is pre-existing and orthogonal to coalescing
    /// (it reproduces on the pre-coalescer engine with a manual delay before the create); the
    /// move-coalescer's create-buffering (P4f-1) makes the window reliable. The edit itself is
    /// NOT buffered — it dispatches immediately (the edit fast-path); only the create-seed's
    /// notification timing is at issue, which the full-sync seeding removes. (Issue 2.)
    ///
    /// Seeds 70/71 reserved.
    #[tokio::test]
    async fn test_local_edit_propagates_to_peer() -> anyhow::Result<()> {
        let node_a = build_node(70).await?;
        let node_b = build_node(71).await?;

        connect_nodes(&node_a, &node_b).await?;

        // Seed the initial file into A's vault BEFORE gossip forms so B receives it via
        // the NeighborUp full sync (no create `ChangeNotification` broadcast).
        node_a.fs.write("notes/flag-edit.md", b"# Original").await?;
        {
            let vault = node_a.vault.lock().await;
            vault.on_file_changed("notes/flag-edit.md").await?;
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

        // Make a real local edit to the file on A's filesystem.
        daemon_a
            .fs
            .write("notes/flag-edit.md", b"# Edited content")
            .await?;

        // Inject the modification event — this is what the OS watcher would deliver.
        inject_modified(&daemon_a, "notes/flag-edit.md");

        // B must receive the updated content.
        wait_until("B has the edited content of notes/flag-edit.md", || {
            let vault = daemon_b.vault.clone();
            async move {
                let vault = vault.lock().await;
                if let Ok(content) = vault.get_document("notes/flag-edit.md").await {
                    content.body().to_string().contains("Edited content")
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

    /// A real local delete tombstones the registry and propagates to a peer.
    ///
    /// This guards the daemon's `on_file_deleted` → `delete_file` (true) → gossip broadcast
    /// path end-to-end: a delete on A tombstones B's copy. `delete_file` is idempotent
    /// (returns `false` for an already-absent path) and the daemon broadcasts only when it
    /// returns `true`; there is no path-keyed sync flag to "arm" anymore.
    ///
    /// The file is seeded via the NeighborUp full sync (not a `Modified` event) so the
    /// deletion's gossip `ChangeNotification{path}` is the FIRST notification for that
    /// path — see `test_file_deletion_propagates` for why a `Modified`-seeded create's
    /// identical notification collides with it in iroh-gossip's content-id dedup once
    /// the create's broadcast is delayed (a pre-existing fragility the move-coalescer's
    /// buffering reliably surfaces; Issue 2 / anti-entropy).
    ///
    /// Seeds 72/73 reserved.
    #[tokio::test]
    async fn test_local_delete_propagates_to_peer() -> anyhow::Result<()> {
        let node_a = build_node(72).await?;
        let node_b = build_node(73).await?;

        connect_nodes(&node_a, &node_b).await?;

        // Seed the file into A's vault BEFORE gossip forms so B receives it via the
        // NeighborUp full sync (no create `ChangeNotification` broadcast).
        node_a
            .fs
            .write("notes/flag-delete.md", b"# To be deleted")
            .await?;
        {
            let vault = node_a.vault.lock().await;
            vault.on_file_changed("notes/flag-delete.md").await?;
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

        // Delete the file on A's filesystem and inject the Deleted event.
        daemon_a.fs.delete("notes/flag-delete.md").await?;
        inject_deleted(&daemon_a, "notes/flag-delete.md");

        // B must tombstone the file.
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
        let mut throttled_hint = PeerRelay::new(
            throttled_id.to_string(),
            "http://example.com:3340/".to_string(),
        );
        throttled_hint.failure_count = 6;
        throttled_hint.last_attempt_ms = Some(now);
        let due_hint = PeerRelay::new(due_id.to_string(), "http://example.com:3340/".to_string());

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
                lookup.get_endpoint_info(x_id).is_none() && lookup.get_endpoint_info(y_id).is_none()
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
        wait_until(
            "A pulled notes/after-net-change.md after the net change",
            || {
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
            },
        )
        .await;

        a_shutdown.cancel();
        daemon_b.shutdown.cancel();
        let _ = a_loop.await;
        let _ = daemon_b.loop_handle.await;

        Ok(())
    }

    /// A successful sync exchange resets the peer's IN-MEMORY hint freshness —
    /// the learn-on-exchange behavior that makes stale-hint eviction safe.
    ///
    /// The supervisor carries a THROTTLED hint for B in its in-memory working set
    /// (high failure_count, recorded attempt). A successful exchange with B fires
    /// `on_exchange_learned`, which stamps the hint and zeroes its failure count
    /// so the supervisor stops backing it off. This is a LAN-direct exchange (no
    /// learned relay URL), the path that used to be `mark_peer_relay_success`.
    ///
    /// The supervisor's working set is now runtime-only (`known_public_relays` is
    /// the sole durable networking store), so the reset is observable in memory,
    /// not on disk — we assert it via `peer_relays_snapshot_for_test()`. We drive
    /// the real `on_exchange_learned` handler directly (via the test seam) rather
    /// than through a spawned `run_loop`, so the daemon stays owned by the test
    /// and its in-memory snapshot is readable. The real run-loop supervisor with a
    /// real swarm is exercised by `net_change_redials_throttled_peer_and_heals_partition`
    /// and the off-LAN-lifeline test; this test's unique value is the reset-on-success.
    ///
    /// Seeds 87/88 reserved (88 only mints B's EndpointId).
    #[tokio::test]
    async fn successful_exchange_resets_hint_freshness() -> anyhow::Result<()> {
        use sync_daemon::persistence::PeerRelay;
        use tempfile::TempDir;

        let node_a = build_node(87).await?;
        // B is never a live node here — only its EndpointId matters, as the hint
        // key and the exchange's reported peer.
        let node_b = build_node(88).await?;
        let b_endpoint = node_b.sync_node.node_id();
        let b_hex = b_endpoint.to_string();

        let vault_dir = TempDir::new()?;
        let vault_path = vault_dir.path().to_path_buf();

        let gossip = node_a
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![])
            .await?;
        let (_file_tx, file_rx) = mpsc::unbounded_channel::<FileEvent>();
        let shutdown = CancellationToken::new();
        let mut daemon_a = Daemon::new(
            node_a.vault.clone(),
            node_a.sync_node,
            gossip,
            file_rx,
            None,
            node_a.allowlist.clone(),
            "device-a".to_string(),
            None,
            vault_path,
            shutdown.clone(),
        );

        // The supervisor's working set starts with a THROTTLED hint for B
        // (failure_count 4, a recorded attempt), as a long-partitioned peer would.
        let mut seeded = PeerRelay::new(b_hex.clone(), "http://example.com:3340/".to_string());
        seeded.failure_count = 4;
        seeded.last_attempt_ms = Some(1_000);
        daemon_a.seed_peer_relays_snapshot(vec![seeded]);

        // A successful LAN-direct exchange with B (no learned relay URL).
        daemon_a
            .apply_exchange_success_for_test(b_endpoint, None)
            .await;

        // The hint's backoff is reset in the supervisor's in-memory snapshot:
        // failure_count back to 0 and a success stamped, so the next tick stops
        // throttling B. The URL is untouched (none was learned).
        let snapshot = daemon_a.peer_relays_snapshot_for_test();
        let hint = snapshot
            .iter()
            .find(|r| r.endpoint_id == b_hex)
            .expect("B's hint should still be present in the supervisor snapshot");
        assert_eq!(
            hint.failure_count, 0,
            "a successful exchange must reset the in-memory failure count"
        );
        assert!(
            hint.last_success_ms.is_some(),
            "a successful exchange must stamp last_success_ms"
        );
        assert_eq!(
            hint.relay_url, "http://example.com:3340/",
            "a LAN-direct exchange (no learned URL) must not change the stored relay URL"
        );

        shutdown.cancel();
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

    /// C6 gossip expansion: a public relay learned from a peer (e.g. a second
    /// server's home relay, discovered once the laptop joins the mesh) is adopted
    /// into the laptop's persisted `known_public_relays` — the cold-bootstrap store
    /// that survives restart, so next launch homes on it too. This is what gives a
    /// laptop failover redundancy across servers it never directly paired with.
    ///
    /// Drives the real learn-on-exchange adoption path with a fabricated learned
    /// relay (the genuine exchange yields a loopback URL, which is correctly
    /// rejected; the cross-network public URL a real second server advertises is
    /// what the adoption path is for), then asserts the persisted config.
    ///
    /// Seed 110 reserved.
    #[tokio::test]
    async fn gossip_learned_public_relay_is_persisted() -> anyhow::Result<()> {
        use sync_daemon::persistence::DaemonConfig;
        use tempfile::TempDir;

        let node = build_node(110).await?;

        let vault_dir = TempDir::new()?;
        let vault_path = vault_dir.path().to_path_buf();

        let gossip = node
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![])
            .await?;
        let (_file_tx, file_rx) = mpsc::unbounded_channel::<FileEvent>();
        let shutdown = CancellationToken::new();
        let mut daemon = Daemon::new(
            node.vault.clone(),
            node.sync_node,
            gossip,
            file_rx,
            None,
            node.allowlist.clone(),
            "device".to_string(),
            None,
            vault_path.clone(),
            shutdown.clone(),
        );

        let learned: iroh::RelayUrl = "https://server2.example.com/".parse().unwrap();
        daemon.learn_public_relay_for_test(&learned).await;

        let (config, _) = DaemonConfig::load_or_generate(&vault_path, None).await?;
        assert!(
            config
                .known_public_relays
                .contains(&"https://server2.example.com/".to_string()),
            "learned public relay should be adopted into known_public_relays; set was {:?}",
            config.known_public_relays
        );

        Ok(())
    }

    /// C6 classifier guard: a learned LAN-IP relay is NEVER adopted into the public
    /// set. A private relay is useless to any peer not on that LAN, so homing on it
    /// would leave a laptop unreachable once it leaves — the exact failure the
    /// public-relay model exists to prevent.
    ///
    /// Seed 111 reserved.
    #[tokio::test]
    async fn gossip_learned_private_relay_is_not_persisted() -> anyhow::Result<()> {
        use sync_daemon::persistence::DaemonConfig;
        use tempfile::TempDir;

        let node = build_node(111).await?;

        let vault_dir = TempDir::new()?;
        let vault_path = vault_dir.path().to_path_buf();

        let gossip = node
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![])
            .await?;
        let (_file_tx, file_rx) = mpsc::unbounded_channel::<FileEvent>();
        let shutdown = CancellationToken::new();
        let mut daemon = Daemon::new(
            node.vault.clone(),
            node.sync_node,
            gossip,
            file_rx,
            None,
            node.allowlist.clone(),
            "device".to_string(),
            None,
            vault_path.clone(),
            shutdown.clone(),
        );

        let learned: iroh::RelayUrl = "http://192.168.68.52:3340/".parse().unwrap();
        daemon.learn_public_relay_for_test(&learned).await;

        let (config, _) = DaemonConfig::load_or_generate(&vault_path, None).await?;
        assert!(
            config.known_public_relays.is_empty(),
            "a learned private LAN-IP relay must not be adopted; set was {:?}",
            config.known_public_relays
        );

        Ok(())
    }

    /// C6 cross-product refresh: a newly-learned public relay means new
    /// `(allowlist peer) × {new relay}` reconnect targets, so the supervisor's
    /// in-memory snapshot gains a `(peer, new_relay)` entry — without it, the
    /// laptop could home on the new relay but would never DIAL its already-trusted
    /// peers through it. The trusted peer comes from the ALLOWLIST (the trust
    /// anchor); the relay is only ever a transport hint paired with it.
    ///
    /// Asserts the user-facing effect via the persisted store plus the snapshot the
    /// supervisor re-dials from. Seeds 112/113 reserved (113 only mints a valid
    /// peer EndpointId for the allowlist).
    #[tokio::test]
    async fn gossip_learned_public_relay_refreshes_cross_product() -> anyhow::Result<()> {
        use sync_daemon::persistence::DaemonConfig;
        use tempfile::TempDir;

        let node = build_node(112).await?;
        // A trusted peer B already in the allowlist (the trust anchor the
        // cross-product enumerates). B is never a live node here — only its
        // EndpointId matters for the (peer × relay) pairing.
        let b_peer = PeerId::from_secret_bytes(super::common::seed(113));
        node.allowlist.add_peer(b_peer, "peer-b").await?;
        let b_hex = b_peer.to_string();

        let vault_dir = TempDir::new()?;
        let vault_path = vault_dir.path().to_path_buf();

        let gossip = node
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![])
            .await?;
        let (_file_tx, file_rx) = mpsc::unbounded_channel::<FileEvent>();
        let shutdown = CancellationToken::new();
        let mut daemon = Daemon::new(
            node.vault.clone(),
            node.sync_node,
            gossip,
            file_rx,
            None,
            node.allowlist.clone(),
            "device".to_string(),
            None,
            vault_path.clone(),
            shutdown.clone(),
        );

        let relay_str = "https://server2.example.com/".to_string();
        let learned: iroh::RelayUrl = relay_str.parse().unwrap();
        daemon.learn_public_relay_for_test(&learned).await;

        // Persisted into the cold store.
        let (config, _) = DaemonConfig::load_or_generate(&vault_path, None).await?;
        assert!(
            config.known_public_relays.contains(&relay_str),
            "learned public relay should be persisted; set was {:?}",
            config.known_public_relays
        );

        // Supervisor snapshot gained the (B, new_relay) reconnect target so a future
        // tick dials B through server2's relay.
        assert!(
            daemon
                .peer_relays_snapshot_for_test()
                .iter()
                .any(|h| h.endpoint_id == b_hex && h.relay_url == relay_str),
            "supervisor snapshot should gain a (peer, new_relay) cross-product entry; \
             snapshot was {:?}",
            daemon.peer_relays_snapshot_for_test()
        );

        Ok(())
    }

    // ── native-move coalescing (P4f-1) ──────────────────────────────────────
    //
    // A native rename surfaces to the daemon as an unlinked `Deleted(old)` +
    // `Modified(new)` (the watcher has no rename linkage — see `watcher.rs`).
    // These tests drive that pair through the live event loop and assert the
    // coalescer collapses it into ONE same-UUID move instead of a tombstone plus a
    // fresh-UUID create. They use the same two-daemon convergence harness as the
    // sync tests above, with simulated `FileEvent`s and in-memory vaults.

    /// The document UUID the index currently records for `path` (as a string), if
    /// any. The headline property of a move is that this UUID is unchanged across
    /// the rename, so the tests read it before and after. Returned as a `String`
    /// purely so the assertions need no `uuid` dependency.
    async fn uuid_of(vault: &Arc<Mutex<Vault<Arc<InMemoryFs>>>>, path: &str) -> Option<String> {
        let vault = vault.lock().await;
        let node = vault.index().node_for_path(path)?;
        vault.index().node_uuid(&node).map(|u| u.to_string())
    }

    /// Simulate a native rename on a daemon's filesystem: the OS atomically moves
    /// `old` → `new`, so afterward `new` holds the content and `old` is gone. The
    /// caller injects the resulting `Deleted(old)` + `Modified(new)` events.
    async fn rename_on_fs(daemon: &TestDaemon, old: &str, new: &str, content: &[u8]) {
        daemon.fs.write(new, content).await.unwrap();
        daemon.fs.delete(old).await.unwrap();
    }

    /// A native rename converges to B with the SAME UUID (delete arrives first —
    /// the common case). The move re-parents the existing node; B detects the
    /// `tree.mov`, re-materializes at the new path, and removes the old one — all
    /// under the document's original UUID, re-transferring zero content.
    #[tokio::test]
    async fn native_rename_delete_first_converges_same_uuid() -> anyhow::Result<()> {
        let node_a = build_node(120).await?;
        let node_b = build_node(121).await?;
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

        // Sync the original file A → B.
        daemon_a.fs.write("notes/old.md", b"# Movable").await?;
        inject_modified(&daemon_a, "notes/old.md");
        wait_until("B has notes/old.md", || {
            let vault = daemon_b.vault.clone();
            async move {
                vault
                    .lock()
                    .await
                    .list_files()
                    .await
                    .unwrap_or_default()
                    .contains(&"notes/old.md".to_string())
            }
        })
        .await;

        // Capture the original UUID on both replicas — they must match end-to-end.
        let uuid_before = uuid_of(&daemon_a.vault, "notes/old.md").await;
        assert!(uuid_before.is_some(), "A should have a node for old.md");
        assert_eq!(
            uuid_of(&daemon_b.vault, "notes/old.md").await,
            uuid_before,
            "B's pre-rename UUID should equal A's"
        );

        // Rename on A's fs, then inject the delete-then-create halves of the move.
        rename_on_fs(&daemon_a, "notes/old.md", "notes/new.md", b"# Movable").await;
        inject_deleted(&daemon_a, "notes/old.md");
        inject_modified(&daemon_a, "notes/new.md");

        // B converges: new path present, old path gone.
        wait_until("B has notes/new.md and not notes/old.md", || {
            let vault = daemon_b.vault.clone();
            async move {
                let files = vault.lock().await.list_files().await.unwrap_or_default();
                files.contains(&"notes/new.md".to_string())
                    && !files.contains(&"notes/old.md".to_string())
            }
        })
        .await;

        // The headline property: the UUID is preserved across the move on both
        // sides — a coalesced rename, NOT a delete + fresh-UUID create.
        assert_eq!(
            uuid_of(&daemon_a.vault, "notes/new.md").await,
            uuid_before,
            "A must keep the original UUID at the new path (move, not re-create)"
        );
        assert_eq!(
            uuid_of(&daemon_b.vault, "notes/new.md").await,
            uuid_before,
            "B must converge to the SAME UUID at the new path"
        );

        // Content intact on B.
        assert_eq!(daemon_b.fs.read("notes/new.md").await?, b"# Movable");

        daemon_a.shutdown.cancel();
        daemon_b.shutdown.cancel();
        let _ = daemon_a.loop_handle.await;
        let _ = daemon_b.loop_handle.await;
        Ok(())
    }

    /// The symmetry proof: the create half can arrive BEFORE the delete half and
    /// still coalesce into the same same-UUID move. This is the case the old
    /// (delete-keyed) design could not express.
    #[tokio::test]
    async fn native_rename_create_first_converges_same_uuid() -> anyhow::Result<()> {
        let node_a = build_node(122).await?;
        let node_b = build_node(123).await?;
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

        daemon_a.fs.write("notes/before.md", b"# Reordered").await?;
        inject_modified(&daemon_a, "notes/before.md");
        wait_until("B has notes/before.md", || {
            let vault = daemon_b.vault.clone();
            async move {
                vault
                    .lock()
                    .await
                    .list_files()
                    .await
                    .unwrap_or_default()
                    .contains(&"notes/before.md".to_string())
            }
        })
        .await;

        let uuid_before = uuid_of(&daemon_a.vault, "notes/before.md").await;
        assert!(uuid_before.is_some());

        // Rename on fs, then inject the CREATE half first, then the delete half.
        rename_on_fs(
            &daemon_a,
            "notes/before.md",
            "notes/after.md",
            b"# Reordered",
        )
        .await;
        inject_modified(&daemon_a, "notes/after.md");
        inject_deleted(&daemon_a, "notes/before.md");

        wait_until("B has notes/after.md and not notes/before.md", || {
            let vault = daemon_b.vault.clone();
            async move {
                let files = vault.lock().await.list_files().await.unwrap_or_default();
                files.contains(&"notes/after.md".to_string())
                    && !files.contains(&"notes/before.md".to_string())
            }
        })
        .await;

        assert_eq!(
            uuid_of(&daemon_a.vault, "notes/after.md").await,
            uuid_before,
            "create-first must still coalesce to a same-UUID move on A"
        );
        assert_eq!(
            uuid_of(&daemon_b.vault, "notes/after.md").await,
            uuid_before,
            "create-first must converge to the same UUID on B"
        );

        daemon_a.shutdown.cancel();
        daemon_b.shutdown.cancel();
        let _ = daemon_a.loop_handle.await;
        let _ = daemon_b.loop_handle.await;
        Ok(())
    }

    /// A `Deleted` with no content-matching create within the window is a REAL
    /// deletion: on window expiry the sweep commits a tombstone and broadcasts, so
    /// B drops the file. Proves the coalescer never swallows a genuine delete.
    ///
    /// The file is seeded to B via the NeighborUp full sync rather than a
    /// `Modified` event, so the deletion's gossip `ChangeNotification{path}` is the
    /// FIRST notification broadcast for that path. (iroh-gossip suppresses a
    /// byte-identical message within a 90s window; a create-notification followed
    /// by a same-path delete-notification collides on its content-derived id — a
    /// pre-existing notification-layer fragility, unrelated to coalescing, that
    /// Issue 2's anti-entropy layer addresses. Seeding via full sync sidesteps it
    /// so this test isolates the coalescer's lone-delete-expiry behavior.)
    #[tokio::test]
    async fn lone_delete_expires_to_real_deletion() -> anyhow::Result<()> {
        let node_a = build_node(124).await?;
        let node_b = build_node(125).await?;
        connect_nodes(&node_a, &node_b).await?;

        // Seed the file into A's vault BEFORE gossip forms, so B receives it via the
        // NeighborUp full sync (no create `ChangeNotification` is broadcast).
        node_a.fs.write("notes/doomed.md", b"goodbye").await?;
        {
            let vault = node_a.vault.lock().await;
            vault.on_file_changed("notes/doomed.md").await?;
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

        wait_until("B has notes/doomed.md via full sync", || {
            let vault = daemon_b.vault.clone();
            async move {
                vault
                    .lock()
                    .await
                    .list_files()
                    .await
                    .unwrap_or_default()
                    .contains(&"notes/doomed.md".to_string())
            }
        })
        .await;

        // Delete with no partner create. The sweep expires it to a tombstone after
        // the window, and the deletion propagates to B.
        daemon_a.fs.delete("notes/doomed.md").await?;
        inject_deleted(&daemon_a, "notes/doomed.md");

        wait_until("B no longer has notes/doomed.md", || {
            let vault = daemon_b.vault.clone();
            async move {
                !vault
                    .lock()
                    .await
                    .list_files()
                    .await
                    .unwrap_or_default()
                    .contains(&"notes/doomed.md".to_string())
            }
        })
        .await;

        daemon_a.shutdown.cancel();
        daemon_b.shutdown.cancel();
        let _ = daemon_a.loop_handle.await;
        let _ = daemon_b.loop_handle.await;
        Ok(())
    }

    /// A `Modified` at a fresh path with no content-matching delete is a REAL new
    /// document: on window expiry the sweep mints a fresh UUID and broadcasts, so
    /// B gains the new file. Proves an unpaired create is never lost.
    #[tokio::test]
    async fn lone_create_expires_to_new_document() -> anyhow::Result<()> {
        let node_a = build_node(126).await?;
        let node_b = build_node(127).await?;
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

        // A genuinely new file (no prior delete in the window).
        daemon_a.fs.write("notes/brand-new.md", b"# Hello").await?;
        inject_modified(&daemon_a, "notes/brand-new.md");

        wait_until("B has notes/brand-new.md", || {
            let vault = daemon_b.vault.clone();
            async move {
                vault
                    .lock()
                    .await
                    .list_files()
                    .await
                    .unwrap_or_default()
                    .contains(&"notes/brand-new.md".to_string())
            }
        })
        .await;

        // It materialized as a normal new document with a real UUID.
        assert!(
            uuid_of(&daemon_a.vault, "notes/brand-new.md")
                .await
                .is_some(),
            "an unpaired create must commit as a real new document with a UUID"
        );
        assert_eq!(daemon_b.fs.read("notes/brand-new.md").await?, b"# Hello");

        daemon_a.shutdown.cancel();
        daemon_b.shutdown.cancel();
        let _ = daemon_a.loop_handle.await;
        let _ = daemon_b.loop_handle.await;
        Ok(())
    }

    /// The EC-8 ancient-coincidence non-adopt: a same-content file appearing LONG
    /// after a deletion (its window already expired) is a NEW document, NOT an
    /// adoption of the dead lineage. Drives the window expiry through the live loop.
    #[tokio::test]
    async fn same_content_after_window_is_new_document_not_adopt() -> anyhow::Result<()> {
        let node_a = build_node(128).await?;
        let node_b = build_node(129).await?;
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

        daemon_a
            .fs
            .write("notes/original.md", b"identical body")
            .await?;
        inject_modified(&daemon_a, "notes/original.md");
        wait_until("B has notes/original.md", || {
            let vault = daemon_b.vault.clone();
            async move {
                vault
                    .lock()
                    .await
                    .list_files()
                    .await
                    .unwrap_or_default()
                    .contains(&"notes/original.md".to_string())
            }
        })
        .await;
        let dead_uuid = uuid_of(&daemon_a.vault, "notes/original.md").await;
        assert!(dead_uuid.is_some());

        // Delete it and let the window fully expire (tombstone commits).
        daemon_a.fs.delete("notes/original.md").await?;
        inject_deleted(&daemon_a, "notes/original.md");
        wait_until("A tombstoned notes/original.md", || {
            let vault = daemon_a.vault.clone();
            async move { uuid_of(&vault, "notes/original.md").await.is_none() }
        })
        .await;

        // A same-content file appears under a different name well after the window.
        // Nothing remains pending to adopt onto → it is a fresh-UUID new document.
        daemon_a
            .fs
            .write("notes/coincidence.md", b"identical body")
            .await?;
        inject_modified(&daemon_a, "notes/coincidence.md");
        wait_until("A has notes/coincidence.md", || {
            let vault = daemon_a.vault.clone();
            async move { uuid_of(&vault, "notes/coincidence.md").await.is_some() }
        })
        .await;

        let new_uuid = uuid_of(&daemon_a.vault, "notes/coincidence.md").await;
        assert!(new_uuid.is_some());
        assert_ne!(
            new_uuid, dead_uuid,
            "a same-content file after the window must be a NEW UUID, not an adoption of the dead lineage"
        );

        daemon_a.shutdown.cancel();
        daemon_b.shutdown.cancel();
        let _ = daemon_a.loop_handle.await;
        let _ = daemon_b.loop_handle.await;
        Ok(())
    }

    /// An in-place edit of an already-tracked file is NOT coalesced — it dispatches
    /// immediately, before any window elapses.
    ///
    /// The proof is A-side and timing-tight: an edit applies SYNCHRONOUSLY in
    /// `on_file_modified`, so A's own document content reflects the edit within a
    /// budget far below the 500ms coalescing window. A buffered edit could not — it
    /// would sit in the window until the sweep (~500-750ms) before applying. Asserting
    /// A's local state (rather than B's converged state) isolates the no-latency
    /// property from QUIC round-trip time, which can itself exceed a few hundred ms.
    #[tokio::test]
    async fn edit_of_tracked_file_is_not_buffered() -> anyhow::Result<()> {
        let node_a = build_node(130).await?;
        let node_b = build_node(131).await?;
        connect_nodes(&node_a, &node_b).await?;

        // Seed v1 into A's vault before gossip forms (no create `ChangeNotification`).
        node_a.fs.write("notes/live.md", b"# v1").await?;
        {
            let vault = node_a.vault.lock().await;
            vault.on_file_changed("notes/live.md").await?;
        }
        let uuid_v1 = uuid_of(&node_a.vault, "notes/live.md").await;

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

        // Edit in place (the path keeps a live node). This must skip the coalescer and
        // apply on A immediately.
        daemon_a.fs.write("notes/live.md", b"# v2 edited").await?;
        inject_modified(&daemon_a, "notes/live.md");

        // A's own document must reflect the edit well inside one coalescing window. A
        // buffered edit would not apply until the sweep — 250ms is a deliberately tight
        // budget below the 500ms window, and A applies with no network in the path.
        let applied = tokio::time::timeout(Duration::from_millis(250), async {
            loop {
                if let Ok(doc) = daemon_a
                    .vault
                    .lock()
                    .await
                    .get_document("notes/live.md")
                    .await
                    && doc.body().to_string().contains("v2 edited")
                {
                    return;
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await;
        assert!(
            applied.is_ok(),
            "an in-place edit must apply immediately on A (not wait for the coalescing window)"
        );

        // Same document — the edit did not mint a new identity (it was never treated as
        // a create-candidate).
        assert_eq!(
            uuid_of(&daemon_a.vault, "notes/live.md").await,
            uuid_v1,
            "an edit keeps the document's UUID"
        );

        // And it still converges to B (the edit's notification is the first for this
        // path, so it is not deduped — see `test_local_edit_propagates_to_peer`).
        wait_until("B has the edited content of notes/live.md", || {
            let vault = daemon_b.vault.clone();
            async move {
                vault
                    .lock()
                    .await
                    .get_document("notes/live.md")
                    .await
                    .map(|d| d.body().to_string().contains("v2 edited"))
                    .unwrap_or(false)
            }
        })
        .await;

        daemon_a.shutdown.cancel();
        daemon_b.shutdown.cancel();
        let _ = daemon_a.loop_handle.await;
        let _ = daemon_b.loop_handle.await;
        Ok(())
    }

    /// OQ-D — a re-create at a JUST-deleted path with identical content. The delete
    /// is still buffered (the node not yet tombstoned), so the matching create
    /// resolves to a same-path move: a no-op that leaves the document alive under
    /// its original UUID, NOT a tombstone-then-fresh-create.
    #[tokio::test]
    async fn recreate_at_deleted_path_keeps_uuid_alive() -> anyhow::Result<()> {
        let node_a = build_node(132).await?;
        let node_b = build_node(133).await?;
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

        daemon_a
            .fs
            .write("notes/churn.md", b"steady content")
            .await?;
        inject_modified(&daemon_a, "notes/churn.md");
        wait_until("B has notes/churn.md", || {
            let vault = daemon_b.vault.clone();
            async move {
                vault
                    .lock()
                    .await
                    .list_files()
                    .await
                    .unwrap_or_default()
                    .contains(&"notes/churn.md".to_string())
            }
        })
        .await;
        let uuid_before = uuid_of(&daemon_a.vault, "notes/churn.md").await;
        assert!(uuid_before.is_some());

        // Delete then immediately re-create the SAME path with the SAME content —
        // within the window, so the delete is still buffered when the create lands.
        daemon_a.fs.delete("notes/churn.md").await?;
        inject_deleted(&daemon_a, "notes/churn.md");
        daemon_a
            .fs
            .write("notes/churn.md", b"steady content")
            .await?;
        inject_modified(&daemon_a, "notes/churn.md");

        // Let the window pass so a (wrongly) un-cancelled delete would have
        // tombstoned by now — then assert the doc is still alive with its UUID.
        tokio::time::sleep(Duration::from_millis(700)).await;
        assert_eq!(
            uuid_of(&daemon_a.vault, "notes/churn.md").await,
            uuid_before,
            "a same-path re-create must cancel the buffered delete and keep the document alive under its UUID"
        );
        assert!(
            daemon_a
                .vault
                .lock()
                .await
                .list_files()
                .await
                .unwrap_or_default()
                .contains(&"notes/churn.md".to_string()),
            "the re-created path must still be present (not tombstoned)"
        );

        daemon_a.shutdown.cancel();
        daemon_b.shutdown.cancel();
        let _ = daemon_a.loop_handle.await;
        let _ = daemon_b.loop_handle.await;
        Ok(())
    }

    /// A same-path create-then-delete both push in real time — the create
    /// notification does not dedup-suppress the follow-up delete notification.
    ///
    /// This is the OPPOSITE of the three reseeded tests (`test_file_deletion_propagates`,
    /// `test_local_edit_propagates_to_peer`, `lone_delete_expires_to_real_deletion`),
    /// which seed the file via the NeighborUp full sync so the delete/edit notification
    /// is the FIRST for its path. Here BOTH notifications go over LIVE gossip in one
    /// seen-cache window: A creates `notes/ping.md` (broadcasts a create-notif), then
    /// deletes the SAME path (broadcasts a delete-notif). On the un-nonced wire format
    /// both `GossipMessage::ChangeNotification` envelopes serialize byte-identically, so
    /// iroh-gossip's content-derived MessageId suppresses the delete-notif within its
    /// ~90s window and B never pulls the deletion (Issue 2). The per-notification nonce
    /// (`salt ^ seq`, distinct per broadcast) makes the bytes differ, so the delete-notif
    /// survives dedup and B drops the file via real-time push — no full-sync reseed.
    ///
    /// Determinism anchor: we `wait_until` B HAS the file before A deletes it. That
    /// guarantees the create-notif reached B's seen-cache, so the delete-notif is
    /// genuinely tested against a populated dedup cache rather than racing ahead and
    /// passing for the wrong reason.
    ///
    /// Seeds 140/141 reserved.
    #[tokio::test]
    async fn same_path_create_then_delete_pushes_in_realtime() -> anyhow::Result<()> {
        let node_a = build_node(140).await?;
        let node_b = build_node(141).await?;

        connect_nodes(&node_a, &node_b).await?;

        // Join gossip LIVE first (A empty-bootstrap, B off A) and do NOT pre-seed the
        // file — the create-notif must travel over live gossip so its MessageId enters
        // iroh-gossip's seen-cache, which is what the delete-notif later collides with.
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

        // A creates the file → broadcasts the create-notif (nonce = salt ^ 0).
        daemon_a.fs.write("notes/ping.md", b"v1").await?;
        inject_modified(&daemon_a, "notes/ping.md");

        // Determinism anchor: B must actually have the file before A deletes it, which
        // proves the create-notif landed in B's seen-cache.
        wait_until("B has notes/ping.md", || {
            let vault = daemon_b.vault.clone();
            async move {
                let vault = vault.lock().await;
                vault
                    .list_files()
                    .await
                    .unwrap_or_default()
                    .contains(&"notes/ping.md".to_string())
            }
        })
        .await;

        // A deletes the SAME path → broadcasts the delete-notif (nonce = salt ^ 1).
        // Un-nonced this serializes identically to the create-notif and is suppressed;
        // with the nonce the bytes differ and B receives it.
        inject_deleted(&daemon_a, "notes/ping.md");

        // The real-time assertion: B drops the file via gossip push, with NO full-sync
        // reseed preceding it — a pass REQUIRES the delete-notif to have survived dedup.
        wait_until("B no longer has notes/ping.md", || {
            let vault = daemon_b.vault.clone();
            async move {
                let vault = vault.lock().await;
                !vault
                    .list_files()
                    .await
                    .unwrap_or_default()
                    .contains(&"notes/ping.md".to_string())
            }
        })
        .await;

        daemon_a.shutdown.cancel();
        daemon_b.shutdown.cancel();
        let _ = daemon_a.loop_handle.await;
        let _ = daemon_b.loop_handle.await;

        Ok(())
    }

    /// The move-coalescer's crash-recovery journal path, relative to the vault
    /// root. Kept as a literal here (rather than importing the daemon's `pub(crate)`
    /// const, which an external integration-test crate can't see) so the test
    /// documents the exact on-disk location it asserts against.
    const PENDING_MOVES_JOURNAL: &str = ".sync/pending-moves.json";

    /// Read the crash-recovery journal back from the in-memory fs the daemon writes
    /// to, and parse it as untyped JSON. The journal's record types are `pub(crate)`,
    /// so the assertions inspect JSON fields rather than the typed struct; what
    /// matters is that these are the bytes the daemon actually persisted, not
    /// in-memory coalescer state. `None` when the file is absent (a clean,
    /// fully-pruned journal leaves no file). Takes a bare fs handle so the
    /// graceful-drain tests, whose post-quit assertions outlive the `TestDaemon`
    /// (awaiting `loop_handle` consumes that field), can read it after teardown.
    async fn read_journal_on_fs(fs: &Arc<InMemoryFs>) -> Option<serde_json::Value> {
        let bytes = fs.read(PENDING_MOVES_JOURNAL).await.ok()?;
        serde_json::from_slice(&bytes).ok()
    }

    /// The `pending` array of the journal, or empty when the file is absent — the
    /// shape the prune assertions check (a committed record leaves no trace).
    async fn journal_pending(daemon: &TestDaemon) -> Vec<serde_json::Value> {
        journal_pending_on_fs(&daemon.fs).await
    }

    /// `journal_pending` against a bare fs handle (see [`read_journal_on_fs`]).
    async fn journal_pending_on_fs(fs: &Arc<InMemoryFs>) -> Vec<serde_json::Value> {
        match read_journal_on_fs(fs).await {
            Some(value) => value
                .get("pending")
                .and_then(|p| p.as_array())
                .cloned()
                .unwrap_or_default(),
            None => Vec::new(),
        }
    }

    /// A buffered move is journaled to `.sync/pending-moves.json` (carrying the
    /// moved doc's UUID, the lineage crash recovery re-stitches) while it sits in the
    /// window, and the record is PRUNED the moment the move commits — proving the
    /// persist-after-commit / prune-from-snapshot contract against the actually
    /// persisted bytes, read back through the daemon's fs.
    #[tokio::test]
    async fn pending_move_is_journaled_then_pruned() -> anyhow::Result<()> {
        let node = build_node(142).await?;
        let gossip = node
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![])
            .await?;
        let daemon = spawn_daemon(node, gossip);

        // Track a file so the delete half resolves to a live node (and a UUID).
        daemon.fs.write("notes/old.md", b"# Movable").await?;
        inject_modified(&daemon, "notes/old.md");
        wait_until("notes/old.md is tracked", || {
            let vault = daemon.vault.clone();
            async move { uuid_of(&vault, "notes/old.md").await.is_some() }
        })
        .await;
        let uuid_before = uuid_of(&daemon.vault, "notes/old.md")
            .await
            .expect("old.md should have a UUID");

        // Rename on the fs, then inject only the DELETE half — it buffers, awaiting
        // its partner create for the window.
        rename_on_fs(&daemon, "notes/old.md", "notes/new.md", b"# Movable").await;
        inject_deleted(&daemon, "notes/old.md");

        // The buffered delete is journaled, carrying the doc's UUID.
        wait_until("the buffered delete is journaled with its UUID", || {
            let daemon = &daemon;
            let uuid_before = uuid_before.clone();
            async move {
                let pending = journal_pending(daemon).await;
                pending.len() == 1
                    && pending[0]["kind"] == "delete"
                    && pending[0]["path"] == "notes/old.md"
                    && pending[0]["uuid"] == serde_json::json!(uuid_before)
            }
        })
        .await;

        // Inject the create half — it pairs into a move, committing the record.
        inject_modified(&daemon, "notes/new.md");

        // The committed record is pruned: the journal's pending set is empty.
        wait_until("the committed move is pruned from the journal", || {
            let daemon = &daemon;
            async move { journal_pending(daemon).await.is_empty() }
        })
        .await;

        // The move preserved the UUID at the new path (not a fresh-UUID re-create).
        assert_eq!(
            uuid_of(&daemon.vault, "notes/new.md").await,
            Some(uuid_before),
            "the coalesced move must keep the original UUID at the new path"
        );

        daemon.shutdown.cancel();
        let _ = daemon.loop_handle.await;
        Ok(())
    }

    /// A lone delete (no partner create) is journaled while it waits, then PRUNED
    /// once its window expires and the sweep commits it standalone (a real
    /// tombstone). Proves the sweep path also persists the post-commit snapshot, so
    /// a standalone-committed record never lingers in the journal to be re-committed
    /// on boot.
    #[tokio::test]
    async fn journal_survives_lone_delete_until_expiry_then_prunes() -> anyhow::Result<()> {
        let node = build_node(143).await?;
        let gossip = node
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![])
            .await?;
        let daemon = spawn_daemon(node, gossip);

        daemon.fs.write("notes/doomed.md", b"goodbye").await?;
        inject_modified(&daemon, "notes/doomed.md");
        wait_until("notes/doomed.md is tracked", || {
            let vault = daemon.vault.clone();
            async move { uuid_of(&vault, "notes/doomed.md").await.is_some() }
        })
        .await;

        // Delete with no partner create — it buffers and is journaled.
        daemon.fs.delete("notes/doomed.md").await?;
        inject_deleted(&daemon, "notes/doomed.md");
        wait_until("the lone delete is journaled", || {
            let daemon = &daemon;
            async move {
                let pending = journal_pending(daemon).await;
                pending.len() == 1 && pending[0]["path"] == "notes/doomed.md"
            }
        })
        .await;

        // The window expires; the sweep commits the delete standalone (the node is
        // tombstoned) AND rewrites the journal from the now-empty snapshot.
        wait_until("the expired delete is pruned from the journal", || {
            let daemon = &daemon;
            async move { journal_pending(daemon).await.is_empty() }
        })
        .await;
        assert!(
            uuid_of(&daemon.vault, "notes/doomed.md").await.is_none(),
            "the lone delete must commit as a real tombstone once its window expires"
        );

        daemon.shutdown.cancel();
        let _ = daemon.loop_handle.await;
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

    // ── P4f-2b-ii: boot move crash-recovery ─────────────────────────────────────
    //
    // The recovery path runs at BOOT, inside `startup_inner`, before the event loop.
    // `build_node`/`spawn_daemon` have no boot-from-disk path (they `Vault::init` a
    // fresh fs), so these tests need a harness that boots a daemon from a pre-staged
    // `InMemoryFs`. `spawn_daemon_from_loaded` runs the EXACT recovery sequence
    // `startup_inner` runs — the same free helpers (`read_pending_journal`,
    // `restitch_inputs`, `clear_pending_journal`) and the same
    // `Daemon::finalize_recovered_journal` — so production and test cannot diverge.
    // The only thing it omits is the network scaffolding (relay/mDNS/watcher) the
    // recovery logic never touches.

    /// Boot a daemon from a PRE-STAGED `InMemoryFs`, running the production recovery
    /// sequence: read journal → build re-stitch inputs → `Vault::load_with_journal`
    /// → `Daemon::new` → `finalize_recovered_journal` → `clear_pending_journal`. The
    /// caller stages `.sync/` + `.loro` + journal + disk into `fs` before calling.
    /// This is `startup_inner`'s recovery path minus relay/mDNS/watcher.
    async fn spawn_daemon_from_loaded(seed_byte: u8, fs: Arc<InMemoryFs>) -> TestDaemon {
        let author = PeerId::from_secret_bytes(super::common::seed(seed_byte));

        // The recovery sequence, identical to `startup_inner`.
        let records = read_pending_journal(fs.as_ref()).await;
        let restitch = restitch_inputs(&records);
        let vault = Vault::load_with_journal(fs.clone(), author.as_u64(), Some(&restitch))
            .await
            .expect("load_with_journal should boot the staged vault");
        let vault = Arc::new(Mutex::new(vault));

        // Build a real SyncNode + gossip exactly as `build_node` does, so the daemon
        // is production-shaped (the recovery broadcast is a real no-op at boot).
        let allowlist = Arc::new(InMemoryAllowlist::new());
        let (inbound_seen_tx, inbound_seen_rx) = mpsc::unbounded_channel();
        let sync_handler = sync_daemon::daemon::PumpedSyncHandler::new(
            vault.clone(),
            allowlist.clone(),
            inbound_seen_tx,
        );
        let sync_node = SyncNode::new_with_sync_handler(
            super::common::seed(seed_byte),
            &[],
            allowlist.clone(),
            sync_handler,
        )
        .await
        .expect("sync node should build");
        let memory_lookup = MemoryLookup::new();
        sync_node
            .endpoint
            .address_lookup()
            .unwrap()
            .add(memory_lookup);

        let gossip = sync_node
            .join_vault_gossip(&shared_vault_id(), vec![])
            .await
            .expect("gossip join should succeed");

        let (file_event_tx, file_event_rx) = mpsc::unbounded_channel::<FileEvent>();
        let shutdown = CancellationToken::new();

        let mut daemon = Daemon::new(
            vault.clone(),
            sync_node,
            gossip,
            file_event_rx,
            None, // no mDNS discovery in tests
            allowlist.clone(),
            "test-device".to_string(),
            None,
            "/test-vault".into(),
            shutdown.clone(),
        );
        daemon.set_inbound_seen_rx(inbound_seen_rx);
        // The daemon's journal fs MUST be the same stateful `InMemoryFs` the vault
        // uses, so the clear lands where `read_pending_journal` read from.
        daemon.set_fs(Arc::new(fs.clone()));

        // Finish recovery before the loop spawns — finalize the unmatched remainder,
        // then clear the journal in one write (the production ordering).
        daemon.finalize_recovered_journal(&records).await;
        clear_pending_journal(fs.as_ref()).await;

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

    /// Write a crash-recovery journal carrying `records` to a staged `InMemoryFs`,
    /// simulating "the daemon crashed with these moves still buffered."
    async fn stage_crash_journal(fs: &Arc<InMemoryFs>, records: Vec<JournaledMove>) {
        let file = PendingMovesFile {
            version: PENDING_MOVES_VERSION,
            pending: records,
        };
        let bytes = serde_json::to_vec(&file).expect("journal serializes");
        fs.atomic_write(PENDING_MOVES_JOURNAL, &bytes)
            .await
            .expect("stage journal write");
    }

    /// Stage the on-disk shape of a native move that crashed mid-window: track `old`
    /// (minting a node + its `<uuid>.loro`), then rename it on disk so `new` holds the
    /// content with NO node, the OLD node is still LIVE in the persisted index, and
    /// `docs/<uuid>.loro` is present. Crucially does NOT tombstone the old node — the
    /// crash window has the delete buffered, not committed. Returns the doc's UUID.
    /// This is the integration sibling of 2b-i's `stage_buffered_move_crash`.
    async fn stage_tracked_then_renamed_on_disk(
        fs: &Arc<InMemoryFs>,
        author: u64,
        old: &str,
        new: &str,
        content: &[u8],
    ) -> Uuid {
        let vault = Vault::init(fs.clone(), author)
            .await
            .expect("staging vault init");
        fs.write(old, content).await.expect("write old");
        vault.on_file_changed(old).await.expect("index old");
        let node = vault
            .index()
            .node_for_path(old)
            .expect("old should have a node after indexing");
        let uuid = vault
            .index()
            .node_uuid(&node)
            .expect("old's node should have a UUID");
        // The disk rename: `new` gets the content, `old` is removed.
        fs.write(new, content).await.expect("write new");
        fs.delete(old).await.expect("delete old");
        // Persist the index so the loaded vault sees the still-live old node.
        vault.save_index().await.expect("save index");
        uuid
    }

    /// Build a fresh `JournaledMove` DELETE record for a staged crash journal, using
    /// the journal's own `hex_lower` encoder so the persisted `content_hash` exactly
    /// matches what the daemon wrote pre-crash (and what the re-stitch will decode).
    fn delete_record(uuid: Uuid, path: &str, hash: [u8; 32]) -> JournaledMove {
        JournaledMove {
            kind: PendingKind::Delete,
            content_hash: hex_lower(&hash),
            path: path.to_string(),
            uuid: Some(uuid.to_string()),
        }
    }

    /// The content hash a move's `.md` lands in reconcile's hash domain under — the
    /// SAME domain the re-stitch matches against (`content_hash(&ContentDoc::from_markdown)`).
    /// The headline test's re-stitch match depends on this exact domain.
    fn hash_of(content: &str, author: u64) -> [u8; 32] {
        content_hash(&ContentDoc::from_markdown(content, author).expect("content doc"))
    }

    /// The HEADLINE end-to-end proof: a native move buffered when the daemon crashed
    /// is re-stitched on the next boot. The boot path reads the journal, feeds the
    /// DELETE record's lineage into load, re-attaches the original UUID at the new
    /// path (NOT a fresh-UUID re-create), and empties the journal.
    #[tokio::test]
    async fn crash_recovery_restitches_buffered_move_on_boot() -> anyhow::Result<()> {
        let fs = Arc::new(InMemoryFs::new());
        let author = PeerId::from_secret_bytes(super::common::seed(200)).as_u64();

        let uuid = stage_tracked_then_renamed_on_disk(
            &fs,
            author,
            "notes/old.md",
            "notes/new.md",
            b"# Movable",
        )
        .await;
        stage_crash_journal(
            &fs,
            vec![delete_record(
                uuid,
                "notes/old.md",
                hash_of("# Movable", author),
            )],
        )
        .await;

        let daemon = spawn_daemon_from_loaded(200, fs).await;

        // Re-stitched: the original UUID is now at the new path, not a fresh mint.
        assert_eq!(
            uuid_of(&daemon.vault, "notes/new.md").await,
            Some(uuid.to_string()),
            "the buffered move must re-stitch the original UUID at the new path"
        );
        assert!(
            uuid_of(&daemon.vault, "notes/old.md").await.is_none(),
            "the old path must be vacated by the re-stitch"
        );
        // The boot path emptied the journal after recovery.
        assert!(
            journal_pending(&daemon).await.is_empty(),
            "the journal must be cleared after boot recovery"
        );

        daemon.shutdown.cancel();
        let _ = daemon.loop_handle.await;
        Ok(())
    }

    /// An unmatched DELETE — a REAL deletion with no content-matching `.md` anywhere —
    /// is finalized as a tombstone by `finalize_recovered_journal`. Reconcile leaves
    /// the still-live old node alone (it never tombstones), so the daemon must commit
    /// the deletion via `on_file_deleted`.
    #[tokio::test]
    async fn boot_finalizes_unmatched_delete_as_tombstone() -> anyhow::Result<()> {
        let fs = Arc::new(InMemoryFs::new());
        let author = PeerId::from_secret_bytes(super::common::seed(201)).as_u64();

        // Track `doomed`, then delete it with NO matching new path — a real deletion.
        let vault = Vault::init(fs.clone(), author).await?;
        fs.write("notes/doomed.md", b"goodbye").await?;
        vault.on_file_changed("notes/doomed.md").await?;
        let uuid = vault
            .index()
            .node_uuid(&vault.index().node_for_path("notes/doomed.md").unwrap())
            .unwrap();
        fs.delete("notes/doomed.md").await?;
        vault.save_index().await?;
        drop(vault);

        stage_crash_journal(
            &fs,
            vec![delete_record(
                uuid,
                "notes/doomed.md",
                hash_of("goodbye", author),
            )],
        )
        .await;

        let daemon = spawn_daemon_from_loaded(201, fs).await;

        assert!(
            uuid_of(&daemon.vault, "notes/doomed.md").await.is_none(),
            "the unmatched delete's still-live node must be tombstoned on boot"
        );
        assert!(
            journal_pending(&daemon).await.is_empty(),
            "the journal must be cleared after boot recovery"
        );

        daemon.shutdown.cancel();
        let _ = daemon.loop_handle.await;
        Ok(())
    }

    /// An unmatched CREATE — an orphaned `.md` with no node and ONLY a create record
    /// (uuid `None`) in the journal — is minted a fresh-UUID node by reconcile's
    /// per-file loop, NOT by any daemon finalize. Proves creates are reconcile's job
    /// (§1): the daemon ignores create records after building the re-stitch inputs.
    #[tokio::test]
    async fn boot_finalizes_unmatched_create_as_new_doc() -> anyhow::Result<()> {
        let fs = Arc::new(InMemoryFs::new());
        let author = PeerId::from_secret_bytes(super::common::seed(202)).as_u64();

        // A vault that exists but has an orphaned `.md` on disk with no node.
        let vault = Vault::init(fs.clone(), author).await?;
        vault.save_index().await?;
        drop(vault);
        fs.write("notes/fresh.md", b"# Fresh").await?;

        // Journal carries ONLY a create record (uuid None — its partner delete never
        // arrived). The daemon should take no explicit action for it.
        stage_crash_journal(
            &fs,
            vec![JournaledMove {
                kind: PendingKind::Create,
                content_hash: hex_lower(&hash_of("# Fresh", author)),
                path: "notes/fresh.md".to_string(),
                uuid: None,
            }],
        )
        .await;

        let daemon = spawn_daemon_from_loaded(202, fs).await;

        assert!(
            uuid_of(&daemon.vault, "notes/fresh.md").await.is_some(),
            "reconcile must mint a fresh-UUID node for the unmatched create"
        );
        assert!(
            journal_pending(&daemon).await.is_empty(),
            "the journal must be cleared after boot recovery"
        );

        daemon.shutdown.cancel();
        let _ = daemon.loop_handle.await;
        Ok(())
    }

    /// A corrupt journal must NOT abort boot. `read_pending_journal` is tolerant —
    /// garbage parses to an empty set — so the daemon boots, a normal file reconciles
    /// to a live node, and the clear rewrites the garbage to a clean empty journal.
    #[tokio::test]
    async fn corrupt_journal_does_not_abort_boot() -> anyhow::Result<()> {
        let fs = Arc::new(InMemoryFs::new());
        let author = PeerId::from_secret_bytes(super::common::seed(203)).as_u64();

        // A normal trackable file plus a garbage journal.
        let vault = Vault::init(fs.clone(), author).await?;
        fs.write("notes/keeper.md", b"# Keeper").await?;
        vault.on_file_changed("notes/keeper.md").await?;
        vault.save_index().await?;
        drop(vault);
        fs.write(PENDING_MOVES_JOURNAL, b"{ this is not json ]]")
            .await?;

        // Boots without panic despite the garbage journal.
        let daemon = spawn_daemon_from_loaded(203, fs).await;

        assert!(
            uuid_of(&daemon.vault, "notes/keeper.md").await.is_some(),
            "the normal file must reconcile to a live node despite a corrupt journal"
        );
        assert!(
            journal_pending(&daemon).await.is_empty(),
            "the clear must rewrite the garbage journal to a clean empty file"
        );

        daemon.shutdown.cancel();
        let _ = daemon.loop_handle.await;
        Ok(())
    }

    /// An absent journal (clean boot) is a no-op: the daemon boots, the tracked file
    /// is unchanged, and no recovery action is taken. The clear still writes an empty
    /// journal even when none existed (harmless).
    #[tokio::test]
    async fn clean_boot_empty_journal_is_noop() -> anyhow::Result<()> {
        let fs = Arc::new(InMemoryFs::new());
        let author = PeerId::from_secret_bytes(super::common::seed(204)).as_u64();

        let vault = Vault::init(fs.clone(), author).await?;
        fs.write("notes/stable.md", b"# Stable").await?;
        vault.on_file_changed("notes/stable.md").await?;
        let uuid_before = vault
            .index()
            .node_for_path("notes/stable.md")
            .and_then(|node| vault.index().node_uuid(&node))
            .map(|u| u.to_string());
        vault.save_index().await?;
        drop(vault);
        // NO journal staged at all.

        let daemon = spawn_daemon_from_loaded(204, fs).await;

        assert_eq!(
            uuid_of(&daemon.vault, "notes/stable.md").await,
            uuid_before,
            "a clean boot must leave the tracked file's UUID unchanged"
        );
        assert!(
            journal_pending(&daemon).await.is_empty(),
            "a clean boot leaves an empty journal"
        );

        daemon.shutdown.cancel();
        let _ = daemon.loop_handle.await;
        Ok(())
    }

    /// Crash-after-restitch-before-clear convergence: a second boot over the same fs
    /// with the SAME delete record re-staged is a clean no-op. The first boot
    /// re-stitched and cleared; re-staging the journal simulates "crashed after
    /// re-stitch, before the clear." The second boot's `finalize_recovered_journal`
    /// finds `old_path` already vacated (no live node) and skips — no double-move, no
    /// error (the §3.1 finalize idempotency).
    #[tokio::test]
    async fn boot_twice_recovery_is_idempotent() -> anyhow::Result<()> {
        let fs = Arc::new(InMemoryFs::new());
        let author = PeerId::from_secret_bytes(super::common::seed(205)).as_u64();

        let uuid = stage_tracked_then_renamed_on_disk(
            &fs,
            author,
            "notes/old.md",
            "notes/new.md",
            b"# Movable",
        )
        .await;
        let record = delete_record(uuid, "notes/old.md", hash_of("# Movable", author));
        stage_crash_journal(&fs, vec![record.clone()]).await;

        // First boot: re-stitches AND clears the journal.
        let daemon1 = spawn_daemon_from_loaded(205, fs.clone()).await;
        assert_eq!(
            uuid_of(&daemon1.vault, "notes/new.md").await,
            Some(uuid.to_string()),
            "first boot re-stitches the move"
        );
        daemon1.shutdown.cancel();
        let _ = daemon1.loop_handle.await;

        // Simulate "crashed after re-stitch, before clear": re-stage the SAME record
        // onto the now-mutated fs (the node already sits at new.md).
        stage_crash_journal(&fs, vec![record]).await;

        // Second boot over the same fs: a clean no-op.
        let daemon2 = spawn_daemon_from_loaded(205, fs).await;
        assert_eq!(
            uuid_of(&daemon2.vault, "notes/new.md").await,
            Some(uuid.to_string()),
            "second boot must NOT double-move — the UUID stays at the new path"
        );
        assert!(
            uuid_of(&daemon2.vault, "notes/old.md").await.is_none(),
            "old path stays vacated on the second boot"
        );
        assert!(
            journal_pending(&daemon2).await.is_empty(),
            "the journal is cleared again after the idempotent second boot"
        );

        daemon2.shutdown.cancel();
        let _ = daemon2.loop_handle.await;
        Ok(())
    }

    // ── P4f-2c: graceful-shutdown drain ─────────────────────────────────────────
    //
    // On a clean quit the run-loop's shutdown arm drains the move-coalescer buffer
    // BEFORE breaking: every buffered record commits standalone (lone delete →
    // tombstone, lone create → fresh-mint doc) and the journal is rewritten empty.
    // The drain is wait-free — pairing is eager, so the buffer holds only unpaired
    // singletons at shutdown, with nothing to wait for. These tests drive a live
    // daemon through `shutdown.cancel()` + `loop_handle.await`; because the drain
    // runs in the shutdown arm before `break`, awaiting the loop guarantees the
    // drain finished, so post-await assertions observe the drained end state.

    /// A clean quit force-expires EVERY buffered record standalone and empties the
    /// journal. Buffer a lone delete (tombstone-bound) and a lone create
    /// (fresh-mint-bound), confirm both are journaled-but-uncommitted, then cancel:
    /// after the loop joins, the delete's node is gone, the create's node is minted,
    /// and the journal is empty. The end-to-end graceful-drain proof (P4f-2c).
    #[tokio::test]
    async fn graceful_shutdown_drains_buffered_records() -> anyhow::Result<()> {
        let node = build_node(160).await?;
        let gossip = node
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![])
            .await?;
        let daemon = spawn_daemon(node, gossip);

        // Track a file so the lone delete resolves to a live node + UUID.
        daemon.fs.write("notes/keep-delete.md", b"goodbye").await?;
        inject_modified(&daemon, "notes/keep-delete.md");
        wait_until("notes/keep-delete.md is tracked", || {
            let vault = daemon.vault.clone();
            async move { uuid_of(&vault, "notes/keep-delete.md").await.is_some() }
        })
        .await;

        // Buffer a LONE DELETE (no partner create injected) — bound to commit as a
        // standalone tombstone at drain.
        daemon.fs.delete("notes/keep-delete.md").await?;
        inject_deleted(&daemon, "notes/keep-delete.md");

        // Buffer a LONE CREATE for a path with no live node — bound to commit as a
        // standalone fresh-UUID document at drain.
        daemon
            .fs
            .write("notes/lone-create.md", b"# Brand new")
            .await?;
        inject_modified(&daemon, "notes/lone-create.md");

        // Both singletons are journaled (buffered, NOT yet committed) before we quit.
        wait_until("both lone records are journaled", || {
            let daemon = &daemon;
            async move { journal_pending(daemon).await.len() == 2 }
        })
        .await;

        // Hold the vault + fs handles past the await on `loop_handle` (which consumes
        // that field), so the post-drain assertions can inspect the drained state.
        let vault = daemon.vault.clone();
        let fs = daemon.fs.clone();

        // Clean quit: the shutdown arm drains the buffer before breaking. Awaiting
        // the loop guarantees the drain (and journal clear) completed.
        daemon.shutdown.cancel();
        let _ = daemon.loop_handle.await;

        // The lone delete committed as a real tombstone — the node is gone.
        assert!(
            uuid_of(&vault, "notes/keep-delete.md").await.is_none(),
            "the graceful drain must commit the lone delete as a standalone tombstone"
        );
        // The lone create committed as a fresh-UUID new document.
        assert!(
            uuid_of(&vault, "notes/lone-create.md").await.is_some(),
            "the graceful drain must commit the lone create as a standalone new doc"
        );
        // The journal is empty — the next boot has zero recovery work.
        assert!(
            journal_pending_on_fs(&fs).await.is_empty(),
            "the graceful drain must leave the journal empty"
        );
        Ok(())
    }

    /// The drain is wait-free: with a lone-delete buffered, the shutdown drain
    /// commits it AND the loop joins well within a generous timeout. A drain that
    /// blocked (waiting on a partner that never arrives) would hit the timeout.
    /// Asserting the drain's EFFECT (the delete is tombstoned + journal emptied) on
    /// top of the timeout is what makes this a wait-free-DRAIN proof and not merely a
    /// "the loop exited" check — the loop joins fast even with no drain, so the
    /// effect assertions are what pin the drain to actually having run.
    #[tokio::test]
    async fn graceful_shutdown_drain_is_wait_free() -> anyhow::Result<()> {
        let node = build_node(161).await?;
        let gossip = node
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![])
            .await?;
        let daemon = spawn_daemon(node, gossip);

        daemon.fs.write("notes/doomed.md", b"goodbye").await?;
        inject_modified(&daemon, "notes/doomed.md");
        wait_until("notes/doomed.md is tracked", || {
            let vault = daemon.vault.clone();
            async move { uuid_of(&vault, "notes/doomed.md").await.is_some() }
        })
        .await;

        // A single buffered lone-delete — the leanest drainable state.
        daemon.fs.delete("notes/doomed.md").await?;
        inject_deleted(&daemon, "notes/doomed.md");
        wait_until("the lone delete is journaled", || {
            let daemon = &daemon;
            async move { journal_pending(daemon).await.len() == 1 }
        })
        .await;

        // Hold the handles past the await on `loop_handle`, which the timeout consumes.
        let vault = daemon.vault.clone();
        let fs = daemon.fs.clone();
        let loop_handle = daemon.loop_handle;
        daemon.shutdown.cancel();

        // The loop must join well within 5s — the drain force-expires the singleton
        // immediately, with no partner-wait to block on. A blocking drain would
        // instead leave the loop running until this timeout fires.
        let joined = tokio::time::timeout(Duration::from_secs(5), loop_handle).await;
        assert!(
            joined.is_ok(),
            "the graceful drain must return without blocking (the loop joined within 5s)"
        );

        // The drain actually RAN within that window: the lone delete is committed as a
        // standalone tombstone and the journal is empty. These pin the wait-free
        // claim to a real, completed drain (the timeout alone passes even with no
        // drain, since the loop exits fast regardless).
        assert!(
            uuid_of(&vault, "notes/doomed.md").await.is_none(),
            "the wait-free drain must have committed the lone delete as a tombstone"
        );
        assert!(
            journal_pending_on_fs(&fs).await.is_empty(),
            "the wait-free drain must have emptied the journal"
        );
        Ok(())
    }

    /// After a graceful drain leaves the journal empty, the NEXT boot over the same
    /// fs does zero recovery work: the drained standalone state stands, and the
    /// journal stays empty. Proves the §0 guarantee — a clean quit leaves the next
    /// boot nothing to recover.
    #[tokio::test]
    async fn boot_after_graceful_drain_finds_empty_journal() -> anyhow::Result<()> {
        let node = build_node(162).await?;
        // Hold a handle to the SAME fs the daemon persists into, to re-boot over it.
        let fs = node.fs.clone();
        let gossip = node
            .sync_node
            .join_vault_gossip(&shared_vault_id(), vec![])
            .await?;
        let daemon = spawn_daemon(node, gossip);

        // Buffer a lone create, drain it on a clean quit (it commits standalone).
        daemon.fs.write("notes/drained-create.md", b"# New").await?;
        inject_modified(&daemon, "notes/drained-create.md");
        wait_until("the lone create is journaled", || {
            let daemon = &daemon;
            async move { journal_pending(daemon).await.len() == 1 }
        })
        .await;
        // Hold the first daemon's vault past the await on `loop_handle`.
        let vault1 = daemon.vault.clone();
        daemon.shutdown.cancel();
        let _ = daemon.loop_handle.await;

        let drained_uuid = uuid_of(&vault1, "notes/drained-create.md").await;
        assert!(
            drained_uuid.is_some(),
            "the drain should have minted the lone create's node before quit"
        );

        // Re-boot a SECOND daemon over the SAME fs (same seed = re-opening the same
        // vault after a restart). The drained empty journal means no recovery work.
        let daemon2 = spawn_daemon_from_loaded(162, fs).await;
        assert_eq!(
            uuid_of(&daemon2.vault, "notes/drained-create.md").await,
            drained_uuid,
            "the next boot keeps the drained standalone create — no recovery churn"
        );
        assert!(
            journal_pending(&daemon2).await.is_empty(),
            "a clean quit leaves the next boot an empty journal"
        );

        daemon2.shutdown.cancel();
        let _ = daemon2.loop_handle.await;
        Ok(())
    }

    // ── inbound-sync freshness signal (S2) ───────────────────────────────────────

    /// The receiver `Daemon::set_inbound_seen_rx` wires is the one the pumped inbound
    /// handler fires on: a peer that completes an inbound sync is reported on that
    /// channel so the run loop can stamp its freshness, which is what lets an
    /// inbound-ONLY peer (one that never initiates) still count as alive (S2).
    ///
    /// Proven end-to-end without a full daemon: drive a real `SYNC_ALPN` handshake
    /// opener into a `PumpedSyncHandler` and assert the paired `inbound_seen_rx` —
    /// the exact receiver `set_inbound_seen_rx` consumes — yields the initiator's
    /// `PeerId`. A regression that drops the handler's `inbound_seen_tx.send` (or
    /// hands the setter a different channel) would leave the receiver empty here.
    #[tokio::test]
    async fn inbound_sync_fires_the_freshness_signal_for_the_wired_receiver()
    -> anyhow::Result<()> {
        // Responder: a node holding the pumped handler, plus the receiver the daemon
        // would wire via `set_inbound_seen_rx`.
        let responder_fs = Arc::new(InMemoryFs::new());
        let responder_author = PeerId::from_secret_bytes(super::common::seed(80));
        let responder_vault =
            Arc::new(Mutex::new(Vault::init(responder_fs.clone(), responder_author.as_u64()).await?));
        let responder_allowlist = Arc::new(InMemoryAllowlist::new());
        let (inbound_seen_tx, mut inbound_seen_rx) = mpsc::unbounded_channel();
        let handler = sync_daemon::daemon::PumpedSyncHandler::new(
            responder_vault.clone(),
            responder_allowlist.clone(),
            inbound_seen_tx,
        );
        let responder = SyncNode::new_with_sync_handler(
            super::common::seed(80),
            &[],
            responder_allowlist.clone(),
            handler,
        )
        .await?;

        // Initiator: a node that opens the handshake. Its vault builds a real
        // `SyncRequest` opener so the responder's handler runs its true path.
        let initiator = build_node(81).await?;
        let initiator_id = initiator.node_id.clone();

        // The handler allowlists once per connection (deny-on-error) — the initiator
        // must be allowed, or the inbound sync is dropped before it fires the signal.
        responder_allowlist
            .add_peer(initiator_id.clone(), "initiator")
            .await?;

        // Teach the initiator how to reach the responder's endpoint.
        let lookup = MemoryLookup::new();
        lookup.add_endpoint_info(responder.endpoint.addr());
        initiator.sync_node.endpoint.address_lookup()?.add(lookup);

        // Open the handshake: connect on `SYNC_ALPN` and write the framed opener. The
        // wire format is sync-core's `[u32 LE len][bytes]` (re-derived here, as the
        // daemon's own `write_frame` does — see `sync_stream.rs`).
        let opener = initiator.vault.lock().await.prepare_request().await?;
        let connection = initiator
            .sync_node
            .endpoint
            .connect(responder.node_id(), sync_core::network::SYNC_ALPN)
            .await?;
        let (mut send, _recv) = connection.open_bi().await?;
        let len = u32::try_from(opener.len()).expect("opener fits in u32");
        send.write_all(&len.to_le_bytes()).await?;
        send.write_all(&opener).await?;
        send.finish()?;

        // The handler processes the opener and fires the initiator's id on the
        // freshness channel. Bound the wait so a regression fails fast.
        let received = tokio::time::timeout(Duration::from_secs(5), inbound_seen_rx.recv())
            .await
            .expect("inbound-seen signal should arrive before the timeout");

        assert_eq!(
            received,
            Some(initiator_id),
            "the wired receiver must yield the initiator's PeerId after an inbound sync"
        );

        // Hold the responder + connection until the assertion completes (dropping the
        // node early would tear the endpoint down before the handler runs).
        drop(connection);
        drop(responder);
        Ok(())
    }
}
