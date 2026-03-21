/**
 * NetworkManager - Coordinates iroh-based peer connections.
 *
 * Wraps WasmSyncNode to manage the lifecycle of P2P connections:
 * - Creates the iroh sync node from the vault's secret key
 * - Joins vault gossip to discover and connect to peers
 * - Polls gossip events and inbound sync requests in a loop
 * - Routes inbound sync messages to WasmVault for processing
 * - Broadcasts file change notifications to connected peers
 * - Manages relay selection: daemon relay if available, plugin relay otherwise
 */

import { Events } from "obsidian";
import { log } from "../logger";
import type { WasmSyncNode, WasmVault } from "../wasm";
import type { IrohRelayServer, ServerStartResult } from "@webdesserts/iroh-relay";
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
  mode: "daemon" | "plugin" | "none";
  /** The relay WebSocket URL, or null if mode is "none". */
  url: string | null;
  /** Number of authenticated clients connected to the plugin relay (0 if daemon or none). */
  clientCount: number;
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

  private relayServer: IrohRelayServer | null = null;
  private daemonDiscovery: DaemonDiscovery | null = null;
  /** The relay URL passed to the sync node at startup. */
  private activeRelayUrl: string | null = null;

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
   * Relay selection order:
   * 1. If vaultAdapter is provided, check daemon.toml for a daemon relay URL.
   * 2. If no daemon relay, start the plugin's embedded relay server.
   * 3. If the plugin relay also fails, continue without relay (LAN-only QUIC).
   *
   * After startup, DaemonDiscovery continues polling so the plugin can log
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

    // 1. Try daemon relay discovery
    let relayUrl: string | undefined;
    if (vaultAdapter) {
      this.daemonDiscovery = new DaemonDiscovery();
      await this.daemonDiscovery.start(vaultAdapter);
      const daemonUrl = this.daemonDiscovery.currentRelayUrl;
      if (daemonUrl) {
        relayUrl = daemonUrl;
        log.info(`Using daemon relay at ${relayUrl}`);
      }
    }

    // 2. If no daemon relay, start the plugin's embedded relay server
    if (!relayUrl) {
      try {
        const { IrohRelayServer: IrohRelayServerClass } = await import("@webdesserts/iroh-relay");
        const server = new IrohRelayServerClass();
        const result: ServerStartResult = await server.start();
        this.relayServer = server;
        relayUrl = result.url;
        log.info(`Plugin relay started at ${relayUrl}`);
      } catch (err) {
        log.warn("Failed to start plugin relay, continuing without relay:", err);
        // Fail-open: sync works via direct QUIC on LAN
      }
    }

    this.activeRelayUrl = relayUrl ?? null;

    // 3. Create the sync node with the chosen relay URL (undefined = RelayMode::Disabled)
    try {
      // Dynamic import to avoid loading WASM at module initialization
      const { WasmSyncNode: WasmSyncNodeClass } = await import("../wasm");
      this.syncNode = await WasmSyncNodeClass.create(secretKey, relayUrl) as WasmSyncNode;
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

    // Subscribe to daemon relay changes that occur after startup.
    // We can't hot-swap the relay URL on a running sync node, but we can
    // stop the plugin relay if the daemon takes over, and log changes.
    if (this.daemonDiscovery) {
      this.daemonDiscovery.on("relay-changed", (url: string | null) => {
        if (url && this.relayServer) {
          // Daemon relay appeared — stop the plugin relay to free the port
          this.relayServer.stop().catch((err) => log.warn("Error stopping plugin relay:", err));
          this.relayServer = null;
          log.info(`Switched to daemon relay at ${url}`);
        } else if (!url && !this.relayServer) {
          // Daemon relay disappeared — plugin relay is gone too. Sync will
          // continue without relay until restart.
          log.info("Daemon relay gone, will start plugin relay on next restart");
        }
        this.trigger("relay-changed", url);
      });
    }

    this.running = true;
    this.pollGossipEvents();
    this.pollInboundSync();
  }

  /**
   * Stop the network manager, shutting down the sync node and relay server.
   */
  async stop(): Promise<void> {
    this.running = false;
    this.connectedPeers.clear();

    if (this.daemonDiscovery) {
      this.daemonDiscovery.stop();
      this.daemonDiscovery = null;
    }

    if (this.relayServer) {
      try {
        await this.relayServer.stop();
      } catch (err) {
        log.warn("Error stopping relay server:", err);
      }
      this.relayServer = null;
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
   * This node's iroh EndpointId (hex string), or null if not started.
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
   * - "plugin": using the plugin's own embedded relay server
   * - "none": no relay is available (LAN-only QUIC)
   */
  get relayInfo(): RelayInfo {
    const daemonUrl = this.daemonDiscovery?.currentRelayUrl ?? null;
    if (daemonUrl) {
      return { mode: "daemon", url: daemonUrl, clientCount: 0 };
    }
    if (this.relayServer) {
      return { mode: "plugin", url: this.activeRelayUrl, clientCount: this.relayServer.clientCount };
    }
    return { mode: "none", url: null, clientCount: 0 };
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
