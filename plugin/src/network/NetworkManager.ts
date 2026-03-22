/**
 * NetworkManager - Coordinates iroh-based peer connections.
 *
 * Wraps WasmSyncNode to manage the lifecycle of P2P connections:
 * - Creates the iroh sync node from the vault's secret key
 * - Joins vault gossip to discover and connect to peers
 * - Polls gossip events and inbound sync requests in a loop
 * - Routes inbound sync messages to WasmVault for processing
 * - Broadcasts file change notifications to connected peers
 *
 * When a daemon is detected via daemon.toml, the plugin defers to the daemon
 * for relay and skips creating its own iroh node. Without a daemon, the plugin
 * creates a WASM iroh node with RelayMode::Disabled (LAN-only direct QUIC).
 */

import { Events } from "obsidian";
import { log } from "../logger";
import type { WasmSyncNode, WasmVault } from "../wasm";
import { DaemonDiscovery } from "./daemon-discovery";

/** Information about a peer currently visible in the gossip swarm */
export interface PeerInfo {
  /** Peer's iroh EndpointId (hex string) */
  id: string;
  /** When the peer was first seen (ms since epoch) */
  connectedAt: Date;
  /** When we last received activity from this peer (ms since epoch) */
  lastActivityAt: Date;
}

/** Information about the relay currently in use. */
export interface RelayInfo {
  /** Where the relay URL came from. "none" means no relay is available. */
  mode: "daemon" | "none";
  /** The relay WebSocket URL, or null if mode is "none". */
  url: string | null;
}

/** Minimal DataAdapter interface needed for daemon.toml discovery. */
interface VaultAdapter {
  exists(path: string): Promise<boolean>;
  read(path: string): Promise<string>;
}

/**
 * Events emitted by NetworkManager:
 * - 'peer-connected': Peer joined the swarm (nodeId: string)
 * - 'peer-disconnected': Peer left the swarm (nodeId: string)
 * - 'change-received': Peer broadcast a file change (from: string, path: string)
 * - 'sync-completed': Completed a sync round-trip with a peer (peerId: string)
 * - 'relay-changed': Relay URL changed (url: string | null)
 */
export class NetworkManager extends Events {
  private syncNode: WasmSyncNode | null = null;
  private running = false;
  private vault: WasmVault | null = null;
  private connectedPeers: Map<string, PeerInfo> = new Map();

  private daemonDiscovery: DaemonDiscovery | null = null;

  /**
   * Set the vault for processing inbound sync messages.
   * Must be called before start().
   */
  setVault(vault: WasmVault): void {
    this.vault = vault;
  }

  /**
   * Start the network manager.
   *
   * If a daemon is detected via daemon.toml, the plugin defers to it: no WASM
   * node is created and no gossip polling runs. The daemon handles all sync.
   *
   * If no daemon is detected, a WASM iroh node is created with
   * RelayMode::Disabled. Peers reachable via direct QUIC on LAN will still sync.
   *
   * DaemonDiscovery continues polling after startup so the plugin can log
   * when the daemon relay appears or disappears.
   *
   * @param secretKey - 32-byte ed25519 secret key for this node
   * @param vaultId - Vault ID (hex string) to join gossip for
   * @param bootstrapPeers - Known peer EndpointIds to bootstrap from
   * @param vaultAdapter - Obsidian DataAdapter used to read daemon.toml
   */
  async start(
    secretKey: Uint8Array,
    vaultId: string,
    bootstrapPeers: string[],
    vaultAdapter?: VaultAdapter,
  ): Promise<void> {
    if (this.running) return;

    // Check for a running daemon with an embedded relay
    let relayUrl: string | undefined;
    if (vaultAdapter) {
      this.daemonDiscovery = new DaemonDiscovery();
      await this.daemonDiscovery.start(vaultAdapter);
      const daemonUrl = this.daemonDiscovery.currentRelayUrl;
      if (daemonUrl) {
        relayUrl = daemonUrl;
      }
    }

    // If daemon is available, defer to it — no WASM node needed
    if (relayUrl) {
      log.info(`Syncing via daemon at ${relayUrl}`);
      this.running = true;

      this.daemonDiscovery?.on("relay-changed", (url: string | null) => {
        if (url) {
          log.info(`Daemon relay detected at ${url}`);
        } else {
          log.info("Daemon relay no longer available");
        }
        this.trigger("relay-changed", url);
      });

      return;
    }

    // No daemon — create WASM node with RelayMode::Disabled
    try {
      // Dynamic import to avoid loading WASM at module initialization
      const { WasmSyncNode: WasmSyncNodeClass } = await import("../wasm");
      this.syncNode = await WasmSyncNodeClass.create(secretKey, undefined) as WasmSyncNode;
      log.info(`Network node created, id=${this.syncNode.nodeId()}`);
    } catch (err) {
      log.error("Failed to create sync node:", err);
      throw err;
    }

    try {
      await this.syncNode!.joinVaultGossip(vaultId, bootstrapPeers);
      log.info(`Joined vault gossip for vault ${vaultId}`);
    } catch (err) {
      log.warn("Failed to join vault gossip:", err);
      // Non-fatal — can still accept inbound connections
    }

    if (this.daemonDiscovery) {
      this.daemonDiscovery.on("relay-changed", (url: string | null) => {
        if (url) {
          log.info(`Daemon relay detected at ${url}`);
        } else {
          log.info("Daemon relay no longer available");
        }
        this.trigger("relay-changed", url);
      });
    }

    this.running = true;
    this.pollGossipEvents();
    this.pollInboundSync();
  }

  /**
   * Stop the network manager, shutting down the sync node.
   */
  async stop(): Promise<void> {
    this.running = false;
    this.connectedPeers.clear();

    if (this.daemonDiscovery) {
      this.daemonDiscovery.stop();
      this.daemonDiscovery = null;
    }

    if (this.syncNode) {
      try {
        await this.syncNode.shutdown();
      } catch (err) {
        log.warn("Error during sync node shutdown:", err);
      }
      this.syncNode = null;
    }
  }

  /**
   * This node's iroh EndpointId (hex string), or null if not started or deferred to daemon.
   */
  get nodeId(): string | null {
    try {
      return this.syncNode?.nodeId() ?? null;
    } catch {
      return null;
    }
  }

  /**
   * Information about the relay currently in use.
   *
   * `mode` reflects where the relay URL came from:
   * - "daemon": using the relay URL from daemon.toml
   * - "none": no relay is available (standalone LAN-only QUIC)
   */
  get relayInfo(): RelayInfo {
    const daemonUrl = this.daemonDiscovery?.currentRelayUrl ?? null;
    if (daemonUrl) {
      return { mode: "daemon", url: daemonUrl };
    }
    return { mode: "none", url: null };
  }

  /**
   * Broadcast a file change notification to all vault peers.
   *
   * Peers who receive the notification will open a QUIC stream to pull
   * the actual sync data via syncWithPeer.
   */
  async broadcastChange(path: string): Promise<void> {
    if (!this.syncNode) return;
    try {
      await this.syncNode.broadcastChange(path);
      log.debug(`Broadcast change notification for ${path}`);
    } catch (err) {
      log.warn(`Failed to broadcast change for ${path}:`, err);
    }
  }

  /**
   * Number of peers currently in the gossip swarm.
   */
  get peerCount(): number {
    return this.connectedPeers.size;
  }

  /**
   * Get the list of peers currently visible in the gossip swarm.
   */
  getConnectedPeers(): PeerInfo[] {
    return Array.from(this.connectedPeers.values());
  }

  // ========== Private Polling Loops ==========

  /**
   * Poll for gossip events in a loop.
   *
   * Handles neighborUp/Down (peer join/leave) and changeReceived
   * (file change notifications from peers). Yields between polls
   * to allow other async work to run.
   */
  private async pollGossipEvents(): Promise<void> {
    while (this.running) {
      const event = this.syncNode?.pollGossipEvent();

      if (event && event !== null) {
        switch (event.type) {
          case "neighborUp": {
            const now = new Date();
            this.connectedPeers.set(event.nodeId, {
              id: event.nodeId,
              connectedAt: now,
              lastActivityAt: now,
            });
            log.info(`Peer joined swarm: ${event.nodeId}`);
            this.trigger("peer-connected", event.nodeId);

            // Initiate a sync with the new peer
            this.syncWithPeer(event.nodeId);
            break;
          }

          case "neighborDown": {
            this.connectedPeers.delete(event.nodeId);
            log.info(`Peer left swarm: ${event.nodeId}`);
            this.trigger("peer-disconnected", event.nodeId);
            break;
          }

          case "changeReceived": {
            // Update last activity timestamp
            const peer = this.connectedPeers.get(event.from);
            if (peer) {
              peer.lastActivityAt = new Date();
            }
            log.debug(`Change notification from ${event.from}: ${event.path}`);
            this.trigger("change-received", event.from, event.path);

            // Pull the updated data from the peer
            this.syncWithPeer(event.from);
            break;
          }
        }
      }

      await new Promise<void>((resolve) => setTimeout(resolve, 50));
    }
  }

  /**
   * Poll for inbound sync requests in a loop.
   *
   * When a peer opens a QUIC stream to us, the message arrives here.
   * We forward it to WasmVault.processSyncMessage and reply with the result.
   */
  private async pollInboundSync(): Promise<void> {
    while (this.running) {
      const request = this.syncNode?.pollInboundSync();

      if (request && request !== null && this.vault) {
        try {
          const result = await this.vault.processSyncMessage(request.messageBytes) as {
            response: Uint8Array | null;
            modifiedPaths: string[];
          };

          if (result.response) {
            this.syncNode?.replyInboundSync(result.response);
          }
          // If null, don't reply — dropping the reply handle closes the stream cleanly

          if (result.modifiedPaths.length > 0) {
            log.info(`${result.modifiedPaths.length} file(s) updated from inbound sync`);
            this.trigger("files-modified", result.modifiedPaths);
          }
        } catch (err) {
          log.error("Failed to process inbound sync request:", err);
        }
      }

      await new Promise<void>((resolve) => setTimeout(resolve, 50));
    }
  }

  /**
   * Initiate a sync round-trip with a specific peer.
   *
   * Prepares a sync request from WasmVault, sends it to the peer via QUIC,
   * and processes the response.
   */
  private async syncWithPeer(peerId: string): Promise<void> {
    if (!this.syncNode || !this.vault) return;

    try {
      const request = await this.vault.prepareSyncRequest();
      const response = await this.syncNode.syncWithPeer(peerId, request);

      const result = await this.vault.processSyncMessage(response) as {
        response: Uint8Array | null;
        modifiedPaths: string[];
      };

      if (result.modifiedPaths.length > 0) {
        log.info(`${result.modifiedPaths.length} file(s) synced from ${peerId}`);
        this.trigger("files-modified", result.modifiedPaths);
      }

      this.trigger("sync-completed", peerId);
    } catch (err) {
      log.warn(`Sync with peer ${peerId} failed:`, err);
    }
  }
}
