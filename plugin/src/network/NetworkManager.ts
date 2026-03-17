/**
 * NetworkManager - Coordinates iroh-based peer connections.
 *
 * Wraps WasmSyncNode to manage the lifecycle of P2P connections:
 * - Creates the iroh sync node from the vault's secret key
 * - Joins vault gossip to discover and connect to peers
 * - Polls gossip events and inbound sync requests in a loop
 * - Routes inbound sync messages to WasmVault for processing
 * - Broadcasts file change notifications to connected peers
 */

import { Events } from "obsidian";
import { log } from "../logger";
import type { WasmSyncNode, WasmVault } from "../wasm";

/** Information about a peer currently visible in the gossip swarm */
export interface PeerInfo {
  /** Peer's iroh EndpointId (hex string) */
  id: string;
  /** When the peer was first seen (ms since epoch) */
  connectedAt: Date;
  /** When we last received activity from this peer (ms since epoch) */
  lastActivityAt: Date;
}

/**
 * Events emitted by NetworkManager:
 * - 'peer-connected': Peer joined the swarm (nodeId: string)
 * - 'peer-disconnected': Peer left the swarm (nodeId: string)
 * - 'change-received': Peer broadcast a file change (from: string, path: string)
 * - 'sync-completed': Completed a sync round-trip with a peer (peerId: string)
 */
export class NetworkManager extends Events {
  private syncNode: WasmSyncNode | null = null;
  private running = false;
  private vault: WasmVault | null = null;
  private connectedPeers: Map<string, PeerInfo> = new Map();

  /** Secret key bytes — stored until start() is called */
  private secretKey: Uint8Array | null = null;
  /** Vault ID for joining gossip */
  private vaultId: string | null = null;
  /** Bootstrap peers for initial gossip connection */
  private bootstrapPeers: string[] = [];

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
   * Creates the iroh sync node, joins vault gossip, and begins polling
   * for events and inbound sync requests.
   *
   * @param secretKey - 32-byte ed25519 secret key for this node
   * @param vaultId - Vault ID (hex string) to join gossip for
   * @param bootstrapPeers - Known peer EndpointIds to bootstrap from
   */
  async start(secretKey: Uint8Array, vaultId: string, bootstrapPeers: string[]): Promise<void> {
    if (this.running) return;

    this.secretKey = secretKey;
    this.vaultId = vaultId;
    this.bootstrapPeers = bootstrapPeers;

    try {
      // Dynamic import to avoid loading WASM at module initialization
      // eslint-disable-next-line @typescript-eslint/no-var-requires
      const { WasmSyncNode: WasmSyncNodeClass } = require("../wasm");
      this.syncNode = await WasmSyncNodeClass.create(secretKey) as WasmSyncNode;
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

          // Reply with the response (even if null, we need to reply to close the stream)
          if (result.response) {
            this.syncNode?.replyInboundSync(result.response);
          } else {
            // Empty response — send zero bytes so the peer's QUIC stream can close
            this.syncNode?.replyInboundSync(new Uint8Array(0));
          }

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
