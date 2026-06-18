//! Startup seam: lock acquisition, vault init, node startup, and public entry points.
//!
//! This module owns `startup_inner`, `StartupBundle`, and the three public entry points
//! (`run_with_shutdown_controlled`, `run_with_shutdown`, `run`). It is the only place
//! that constructs a real `NativeFs` / `FileAllowlistStorage` `Daemon`.

use anyhow::{Context, Result};
use iroh::{EndpointId, RelayUrl};
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, broadcast, mpsc, watch};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;
use tracing::{info, warn};

use sync_core::fs::FileSystem;
use sync_core::network::SyncNode;
use sync_core::network::discovery::MeshMetadata;
use sync_core::allowlist::AllowlistStorage;
use sync_core::Vault;

use crate::allowlist::FileAllowlistStorage;
use crate::daemon_lock::DaemonLock;
use crate::http;
use crate::native_fs::NativeFs;
use crate::pair_api::{
    DaemonControl, DaemonStatus, PAIRING_BROADCAST_CAPACITY,
};
use crate::persistence::{DaemonConfig, PeerRelay};
use crate::relay::EmbeddedRelay;
use crate::watcher::FileWatcher;

use super::Daemon;

/// Start the daemon and return a control handle before the event loop begins.
///
/// Unlike `run_with_shutdown`, this function splits startup from the event loop:
///
/// - The **outer `Result`** covers everything up to and including `Daemon::new()` —
///   lock acquire, vault load, identity load, relay start, SyncNode creation, mDNS
///   publish, gossip join, health endpoint, file watcher. Startup failures bubble up
///   as `Err` before any `DaemonControl` is materialized.
/// - **`DaemonControl`** is yielded after `Daemon::new()` returns successfully, giving
///   the caller a way to observe status and drive pairing.
/// - The **inner `JoinHandle<Result<()>>`** owns the event loop and graceful shutdown.
///   Its `Result` covers only runtime errors (not startup failures).
///
/// If `shutdown` fires during startup, the function returns `Ok(...)` with the join
/// handle already resolved — callers can safely await it.
pub async fn run_with_shutdown_controlled(
    config: super::DaemonRunConfig,
    shutdown: CancellationToken,
) -> Result<(DaemonControl, JoinHandle<Result<()>>)> {
    // Run startup. If `shutdown` fires before startup completes, return a no-op handle.
    let startup_result = tokio::select! {
        result = startup_inner(&config, shutdown.clone()) => result,
        _ = shutdown.cancelled() => {
            // Cancel during startup — DaemonLock cleanup is RAII (dropped with the future).
            info!("Daemon shutdown requested during startup — exiting cleanly");

            // Best-effort: if startup_inner wrote relay_url to daemon.toml before the
            // future was cancelled (it writes synchronously after the relay starts, then
            // awaits SyncNode::new), clear it now. Without this, the file is left with a
            // stale relay_url that peers could read between this cancelled run and the next
            // startup (which also clears it, but only after re-starting).
            let config_path = config.vault.join(".sync/daemon.toml");
            if config_path.exists() {
                match std::fs::read_to_string(&config_path)
                    .map_err(anyhow::Error::from)
                    .and_then(|s| toml::from_str::<DaemonConfig>(&s).map_err(Into::into))
                {
                    Ok(mut cfg) if cfg.relay_url.is_some() => {
                        if let Err(e) = cfg.set_relay_url(None, &config.vault) {
                            warn!("Failed to clear relay URL after mid-startup cancel: {}", e);
                        }
                    }
                    Ok(_) => {} // relay_url already absent — nothing to do
                    Err(e) => {
                        warn!("Could not parse daemon.toml to clear relay URL after cancel: {}", e);
                    }
                }
            }

            // Return a no-op handle that resolves immediately.
            let handle = tokio::spawn(async { Ok(()) });
            let (status_tx, status_rx) = watch::channel(DaemonStatus::initial());
            let (pairing_tx, pairing_rx) = broadcast::channel(PAIRING_BROADCAST_CAPACITY);
            let (command_tx, _command_rx) = mpsc::unbounded_channel();
            drop(status_tx);
            drop(pairing_tx);
            let control = DaemonControl { status_rx, pairing_rx, command_tx };
            return Ok((control, handle));
        }
    };

    let StartupBundle {
        mut daemon,
        embedded_relay,
        mut daemon_config,
        vault_path,
        mesh_name,
        _daemon_lock,
        _watcher,
    } = startup_result?;

    // Wire control channels into the daemon before the loop starts.
    let (status_tx, status_rx) = watch::channel(DaemonStatus::initial());
    let (pairing_tx, pairing_rx) = broadcast::channel(PAIRING_BROADCAST_CAPACITY);
    let (command_tx, command_rx) = mpsc::unbounded_channel();

    daemon.wire_control(status_tx, pairing_tx.clone(), command_rx, mesh_name);

    // Emit the initial status (Idle, 0 peers, relay URL set) so the tray has something
    // to display immediately without waiting for the first peer event.
    daemon.emit_status().await;

    let control = DaemonControl {
        status_rx,
        pairing_rx,
        command_tx,
    };

    let handle = tokio::spawn(async move {
        // DaemonLock and FileWatcher are moved here so they live for the duration
        // of run_loop. The underscore prefix silences the "unused" warning while
        // making the RAII intent explicit — both must not drop before run_loop exits.
        let _daemon_lock = _daemon_lock;
        let _watcher = _watcher;

        daemon.run_loop().await;
        // Graceful shutdown — sync_node.shutdown() consumes the node;
        // daemon is owned here (moved into this closure) so this is safe.
        if let Err(e) = daemon.sync_node.shutdown().await {
            warn!("Error during iroh node shutdown: {}", e);
        }
        if let Some(relay) = embedded_relay {
            relay.shutdown().await;
            if let Err(e) = daemon_config.set_relay_url(None, &vault_path) {
                warn!("Failed to clear relay URL from daemon.toml: {}", e);
            }
        }
        Ok(())
    });

    Ok((control, handle))
}

/// Run the sync daemon with the given configuration, honoring an externally-supplied
/// cancellation token.
///
/// Cancelling `shutdown` at any point — including during startup — causes the function
/// to return cleanly. Delegates to [`run_with_shutdown_controlled`]; the returned
/// [`DaemonControl`] handle is dropped immediately (no tray or UI consumer in the
/// headless path).
pub async fn run_with_shutdown(config: super::DaemonRunConfig, shutdown: CancellationToken) -> Result<()> {
    let (_control, handle) = run_with_shutdown_controlled(config, shutdown).await?;
    // Drop _control: its receivers/sender have no consumer in the headless path.
    // Sends in the run loop are fire-and-forget (watch::Sender and broadcast::Sender
    // both discard when there are no receivers), so dropping here is safe.
    info!("Daemon running. Press Ctrl+C to stop.");
    handle.await?
}

/// Run the sync daemon with the given configuration.
///
/// This is the main entry point for embedding the daemon in the `memory` binary.
/// Assumes logging is already configured by the caller.
pub async fn run(config: super::DaemonRunConfig) -> Result<()> {
    let token = CancellationToken::new();
    let signal_token = token.clone();
    tokio::spawn(async move {
        memory_common::shutdown_signal().await;
        signal_token.cancel();
    });
    run_with_shutdown(config, token).await
}

/// Everything `startup_inner` returns to the caller after a successful startup.
///
/// Keeps `DaemonLock` and `FileWatcher` alive until they are explicitly moved into
/// the `run_loop` task — if either were dropped at the call site, the OS-level lock
/// and file watcher would stop before the daemon processes any events.
struct StartupBundle {
    daemon: Daemon<NativeFs, FileAllowlistStorage>,
    embedded_relay: Option<EmbeddedRelay>,
    daemon_config: DaemonConfig,
    vault_path: PathBuf,
    mesh_name: String,
    /// Holds the exclusive flock on `.sync/daemon.lock` for the daemon's lifetime.
    _daemon_lock: DaemonLock,
    /// Keeps the OS-level file watcher alive; dropping it stops event delivery.
    _watcher: FileWatcher,
}

/// Startup phase: lock acquisition, vault init, node startup.
///
/// Returns a [`StartupBundle`] that includes `DaemonLock` and `FileWatcher` so
/// the caller can move them into the spawned `run_loop` task, keeping both alive
/// for the daemon's full lifetime. All startup failures surface as `Err` before
/// any `StartupBundle` is materialized.
///
/// Used by `run_with_shutdown_controlled`; the event loop is run by the caller.
async fn startup_inner(
    config: &super::DaemonRunConfig,
    shutdown: CancellationToken,
) -> Result<StartupBundle> {
    // Acquire exclusive daemon lock — must outlive this function. It is moved into
    // the StartupBundle and from there into the spawned run_loop task.
    let daemon_lock = DaemonLock::acquire(&config.vault).context(
        "Failed to acquire daemon lock — is another daemon already running on this vault?",
    )?;

    // Seed the process-global test-time-scale from the env var, once, before any
    // scaled duration is constructed below. Unset → never seeded → scale stays
    // 1.0 → production timing is unchanged. This is the ONLY place the env var is
    // read; the desktop app and WASM plugin never seed it.
    if let Ok(raw) = std::env::var("OBSIDIAN_MEMORY_TIME_SCALE") {
        match raw.parse::<f64>() {
            Ok(scale) if sync_core::time_scale::set_time_scale(scale) => {
                info!(
                    scale,
                    "OBSIDIAN_MEMORY_TIME_SCALE active — durations scaled (test mode)"
                );
            }
            Ok(_) => {}
            Err(_) => warn!(raw = %raw, "Ignoring unparseable OBSIDIAN_MEMORY_TIME_SCALE"),
        }
    }

    info!("Starting sync daemon");
    info!("Vault path: {:?}", config.vault);

    let fs = NativeFs::new(config.vault.clone());

    // Load identity before the vault so we can author Loro ops under this
    // device's PeerId (config-load has no dependency on the vault).
    let (mut daemon_config, identity_key) =
        DaemonConfig::load_or_generate(&config.vault, config.identity_key.as_deref()).await?;

    info!("Daemon PeerId: {}", daemon_config.peer_id);

    let author = identity_key.peer_id();
    let vault = if fs.exists(".sync").await? {
        info!("Loading existing vault");
        Vault::load(fs, author).await?
    } else {
        info!("Initializing new vault");
        Vault::init(fs, author).await?
    };

    let vault_id = vault.vault_id();
    info!("Vault loaded, vault ID: {}", vault_id);

    if daemon_config.relay_url.is_some() {
        info!("Clearing stale relay URL from previous run");
        if let Err(e) = daemon_config.set_relay_url(None, &config.vault) {
            warn!("Failed to clear stale relay URL: {}", e);
        }
    }

    // Start the embedded relay before the SyncNode so we can pass its URL in.
    // When advertised_relay_url is set, bind on relay_listen but tell peers to dial
    // the advertised address (e.g. LAN IP instead of 0.0.0.0).
    // Failure is non-fatal: the daemon continues without relay support.
    let embedded_relay: Option<EmbeddedRelay> = if let Some(ref addr_str) = config.relay_listen {
        match addr_str.parse() {
            Ok(bind_addr) => {
                let relay_result = if let Some(ref adv_url) = config.advertised_relay_url {
                    EmbeddedRelay::start_with_advertised_url(bind_addr, adv_url).await
                } else {
                    EmbeddedRelay::start(bind_addr).await
                };
                match relay_result {
                    Ok(relay) => {
                        info!(url = %relay.relay_url(), "Embedded relay started");
                        Some(relay)
                    }
                    Err(e) => {
                        warn!("Failed to start embedded relay, continuing without: {}", e);
                        None
                    }
                }
            }
            Err(e) => {
                warn!(
                    "Invalid relay-listen address '{}': {}, continuing without relay",
                    addr_str, e
                );
                None
            }
        }
    } else {
        None
    };

    let relay_url = embedded_relay.as_ref().map(|r| r.relay_url().clone());

    if let Some(ref url) = relay_url {
        if let Err(e) = daemon_config.set_relay_url(Some(url.to_string()), &config.vault) {
            warn!("Failed to persist relay URL to daemon.toml: {}", e);
        }

        // A SERVER's own relay belongs in `known_public_relays` too: that set is
        // "all public relays this node knows," and this node IS one of them. Without
        // this, a server only ever learns OTHERS' relays (via pairing-adopt), so its
        // own set stays empty → its cross-product `(allowlist × known_public_relays)`
        // is empty → its reconnect supervisor sits idle with no dial targets after a
        // restart. Seeding the own relay gives e.g. umbra a `(laptops) × {umbra}`
        // cross-product so it can re-dial laptops through its own relay (bidirectional
        // recovery). `add_known_public_relay`'s off-LAN-reachable guard keeps a
        // loopback-bound relay out (correctly — it's no use as a dial target), and it
        // dedups, so this is a no-op once the public domain is already present.
        daemon_config.add_known_public_relay(&url.to_string());
        if let Err(e) = daemon_config.save(&config.vault) {
            warn!("Failed to persist own relay into known_public_relays: {}", e);
        }
    }

    let secret_key_bytes = identity_key.secret_key_bytes();
    let allowlist = Arc::new(FileAllowlistStorage::new(&config.vault));

    // Parse the persisted public-relay set once — it feeds BOTH wires below:
    // this node's RelayMap (a laptop's home/failover) and, crossed with the
    // allowlist, the live peer_lookup + supervisor snapshot. Malformed entries
    // are skipped with a warning rather than failing startup.
    let public_relays: Vec<RelayUrl> = daemon_config
        .known_public_relays
        .iter()
        .filter_map(|url| match url.parse::<RelayUrl>() {
            Ok(parsed) => Some(parsed),
            Err(e) => {
                warn!(relay_url = %url, "Skipping known public relay — invalid URL: {e}");
                None
            }
        })
        .collect();

    // The set of relays this node homes on (its `RelayMap`).
    //
    // A SERVER (embedded relay running → `relay_url.is_some()`) homes on its OWN
    // public relay: it IS a relay, so it must be present in its own RelayMap to be
    // reachable. This is independent of `known_public_relays` — a server with an
    // empty public set still homes on its own relay (a server whose RelayMap ended
    // up `Disabled` would be unreachable, breaking the whole mesh).
    //
    // A LAPTOP (no embedded relay) homes on the learned `known_public_relays` set,
    // so it homes on a public relay it can reach off-LAN (and fails over across the
    // set). An empty set → `RelayMode::Disabled` (LAN-only — the never-met-a-server
    // edge).
    let home_relays: Vec<RelayUrl> = match relay_url.as_ref() {
        Some(own) => vec![own.clone()],
        None => public_relays.clone(),
    };
    let sync_node = SyncNode::new(secret_key_bytes, &home_relays, allowlist.clone())
        .await
        .context("Failed to create iroh SyncNode")?;

    info!(node_id = %sync_node.node_id(), "Iroh node started");

    // Seed the live peer_lookup + the supervisor's working snapshot from the
    // CROSS-PRODUCT `(allowlist peers) × (known_public_relays)`. There is no
    // persisted per-peer relay hint anymore: to reach a paired peer off-LAN we try
    // its EndpointId through each known public relay (a peer registered on a public
    // relay is reached by dialing it through that relay). On-LAN peers are still
    // found live by mDNS, so they need no hint here.
    //
    // Two-wires separation: `known_public_relays` → this node's RelayMap (above);
    // `(allowlist × known_public_relays)` → peer_lookup + supervisor snapshot (here).
    //
    // SECURITY: the EndpointIds come from the ALLOWLIST (the trust anchor); the
    // relay URLs are untrusted transport hints. A relay never contributes or
    // changes an EndpointId — it is only ever paired with already-trusted IDs.
    // `connect(EndpointAddr::new(id).with_relay_url(url))` TLS-verifies `id`
    // regardless of `url`, so a hostile relay is DoS/metadata only, never
    // impersonation.
    //
    // Built HERE (before `bootstrap_ids` moves into `join_vault_gossip` and the
    // `allowlist` Arc moves into `Daemon::new`) so the same cross-product feeds
    // both wires, computed once. The resulting `Vec<PeerRelay>` is handed to
    // `seed_peer_relays_snapshot` after `Daemon::new`.
    let own_endpoint_id = sync_node.node_id();
    let supervisor_seed: Vec<PeerRelay> = {
        let allowlist_peers = match allowlist.list_peers().await {
            Ok(peers) => peers,
            Err(e) => {
                warn!("Failed to read allowlist for peer-relay seed: {}", e);
                vec![]
            }
        };
        let mut seed = Vec::new();
        for peer in &allowlist_peers {
            let endpoint_id = match EndpointId::from_bytes(peer.node_id.as_bytes()) {
                Ok(id) => id,
                Err(e) => {
                    warn!("Skipping invalid allowlist peer for peer-relay seed: {e}");
                    continue;
                }
            };
            // Skip self: the first-pair bootstrap adds this node to its OWN
            // allowlist, but seeding ourselves would make the supervisor (and
            // gossip bootstrap) try to dial this node through a relay — iroh
            // rejects self-directed relay paths. Mirrors `add_peer_relay`'s and
            // `upsert_peer_relay`'s self-skip; the old persisted-`peer_relays` seed
            // never contained self because `upsert_peer_relay` filtered it.
            if endpoint_id == own_endpoint_id {
                continue;
            }
            let endpoint_hex = peer.node_id.to_string();
            for relay in &public_relays {
                sync_node.add_peer_relay(endpoint_id, relay);
                seed.push(PeerRelay::new(endpoint_hex.clone(), relay.to_string()));
            }
        }
        seed
    };
    if !supervisor_seed.is_empty() {
        info!(
            count = supervisor_seed.len(),
            "Seeded peer_lookup from (allowlist × known_public_relays)"
        );
    }

    let mesh_name = daemon_config.mesh_name.clone().unwrap_or_else(|| {
        config
            .vault
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("Obsidian Vault")
            .to_string()
    });
    let mesh_metadata = MeshMetadata {
        mesh: mesh_name.clone(),
        vid: vault_id.to_string(),
        ver: 1,
    };
    sync_node.publish_mesh_info(&mesh_metadata, relay_url.as_ref());

    let bootstrap_ids: Vec<EndpointId> = match allowlist.list_peers().await {
        Ok(peers) => peers
            .iter()
            .filter_map(|p| {
                EndpointId::from_bytes(p.node_id.as_bytes())
                    .map_err(|e| {
                        warn!(
                            "Skipping invalid allowlist peer for gossip bootstrap: {}",
                            e
                        )
                    })
                    .ok()
            })
            .collect(),
        Err(e) => {
            warn!("Failed to read allowlist for gossip bootstrap: {}", e);
            vec![]
        }
    };

    let vault_gossip = sync_node
        .join_vault_gossip(&vault_id, bootstrap_ids)
        .await
        .context("Failed to join vault gossip topic")?;

    info!("Joined vault gossip topic");

    if let Some(ref health_addr) = config.health_listen {
        let health_addr = health_addr.clone();
        tokio::spawn(async move {
            http::serve_health(&health_addr).await;
        });
        info!(
            "Health endpoint started on {}",
            config.health_listen.as_ref().unwrap()
        );
    }

    let watcher = FileWatcher::new(config.vault.clone())?;
    info!("File watcher started");

    let discovery_rx: Option<
        tokio::sync::mpsc::Receiver<sync_core::network::discovery::DiscoveredMesh>,
    > = {
        use futures::StreamExt;
        use sync_core::network::discovery::{DiscoveryEvent, mesh_from_discovery_event};

        if let Some(stream) = sync_node.subscribe_discovery().await {
            let (tx, rx) = tokio::sync::mpsc::channel(16);
            tokio::spawn(async move {
                futures::pin_mut!(stream);
                while let Some(event) = stream.next().await {
                    match event {
                        DiscoveryEvent::Discovered { .. } => {
                            if let Some(mesh) = mesh_from_discovery_event(&event) {
                                let _ = tx.try_send(mesh);
                            }
                        }
                        DiscoveryEvent::Expired { endpoint_id } => {
                            tracing::debug!(peer = %endpoint_id, "mDNS: peer expired");
                        }
                    }
                }
            });
            Some(rx)
        } else {
            None
        }
    };

    let device_name = {
        let hostname = gethostname::gethostname();
        hostname.to_str().unwrap_or("Sync Daemon").to_string()
    };
    info!(device_name = %device_name, "Device name resolved for pairing");

    // The watcher must outlive this function — it is moved into StartupBundle and
    // from there into the spawned run_loop task. Dropping it would stop OS events.
    let (file_event_rx, watcher) = watcher.into_event_rx();

    let mut daemon = Daemon::new(
        Arc::new(Mutex::new(vault)),
        sync_node,
        vault_gossip,
        file_event_rx,
        discovery_rx,
        allowlist,
        device_name,
        relay_url.as_ref().map(|u| u.to_string()),
        config.vault.clone(),
        shutdown,
    );

    // Give the reconnect supervisor its starting address book — the same
    // cross-product `(allowlist × known_public_relays)` the live lookup was just
    // seeded with. After a partition, the supervisor re-bootstraps gossip from this
    // snapshot without a restart. (The freshness fields are runtime-only state, so
    // a restart resetting backoff is fine — nothing here is read from disk.)
    daemon.seed_peer_relays_snapshot(supervisor_seed);

    // Network-change detection: iroh's watch_addr fires when the endpoint's relay
    // or direct addresses change (i.e. the network changed). Forward each change
    // as a () so the run_loop can reset reconnect backoff and re-dial without a
    // restart. The endpoint is read here, after Daemon::new took ownership of
    // sync_node, but before daemon is moved into StartupBundle.
    //
    // We are only ONE consumer of this signal. iroh consumes the same change
    // internally to rebind sockets, re-home its relay, and re-publish our updated
    // EndpointAddr over gossip (the warm re-announce — see `join_vault_gossip`). So
    // do NOT add manual re-advertising of the iroh-level EndpointAddr on the GOSSIP
    // channel — iroh already does that, and only for peers we're already connected to.
    // That is a DIFFERENT channel from our mDNS service registration, which
    // `on_network_change` DOES kick (republish addresses + restart browse) to
    // re-establish LAN discovery with peers we got partitioned from on the move —
    // the one thing the gossip re-announce can't reach. Our other job here is to
    // re-inject gossip bootstrap (reset backoff → the supervisor re-dials), since a
    // partition empties gossip's peer views.
    {
        use futures::StreamExt;
        use iroh::Watcher;
        // Capacity 1: a single wifi switch emits several address changes; they
        // coalesce into one backoff reset (resetting twice is a no-op).
        let (tx, rx) = tokio::sync::mpsc::channel::<()>(1);
        // stream_updates_only (not stream) skips the current address, so we react
        // only to real changes — no spurious reset at startup.
        let mut addr_stream = daemon.sync_node.endpoint.watch_addr().stream_updates_only();
        tokio::spawn(async move {
            while addr_stream.next().await.is_some() {
                // try_send: if a reset is already queued, dropping the extra change
                // is correct — the handler re-dials ALL hints regardless of how
                // many changes fired. A closed channel (receiver gone, i.e. daemon
                // shut down) ends the task.
                if tx.try_send(()).is_err() && tx.is_closed() {
                    break;
                }
            }
        });
        daemon.set_net_change_rx(rx);
    }

    Ok(StartupBundle {
        daemon,
        embedded_relay,
        daemon_config,
        vault_path: config.vault.clone(),
        mesh_name,
        _daemon_lock: daemon_lock,
        _watcher: watcher,
    })
}
