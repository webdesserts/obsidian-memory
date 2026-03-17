/**
 * PeerManager - Coordinates WebSocket server and client connections.
 *
 * Manages the lifecycle of P2P connections:
 * - Starts a WebSocket server to accept incoming connections
 * - Connects to other peers as a client
 * - Routes messages between peers and the WASM sync engine
 *
 * State management is delegated to Rust (PeerRegistry via WasmVault).
 * PeerManager only holds:
 * - WebSocket handles (can't cross WASM boundary)
 * - Cached peer IDs for message routing (avoids WASM call per message)
 */

import { Events, Platform } from "obsidian";
import { SyncWebSocketClient } from "./WebSocketClient";
import { log } from "../logger";
import type { ConnectedPeer, DisconnectReason } from "../wasm";

// Type for dynamically loaded WebSocket server
interface SyncWebSocketServer {
  start(options: { port: number; maxRetries?: number }): Promise<number>;
  stop(): Promise<void>;
  on(event: string, callback: (...args: any[]) => void): void;
  send(peerId: string, data: Uint8Array): void;
  broadcast(data: Uint8Array): void;
  disconnect(peerId: string): void;
  readonly port: number;
  readonly isRunning: boolean;
}

/** Default port for the WebSocket server */
const DEFAULT_PORT = 8765;

/**
 * Interface for vault peer management methods.
 * PeerManager uses this to notify Rust of connection events.
 */
export interface VaultPeerManager {
  peerConnecting(connectionId: string, address: string, direction: string): ConnectedPeer;
  peerHandshakeComplete(connectionId: string, peerId: string): ConnectedPeer;
  peerDisconnected(id: string, reason: DisconnectReason): void;
  resolvePeerId(connectionId: string): string;
  getKnownPeers(): ConnectedPeer[];
  getConnectedPeers(): ConnectedPeer[];
}

/**
 * A connection entry holding socket and cached peer ID.
 * peerId is cached after handshake to avoid WASM calls per message.
 */
interface Connection {
  socket: SyncWebSocketClient | null; // null for incoming (server manages)
  peerId?: string; // Cached after handshake for message routing
}

/** Information about a connected peer (for events and display) */
export interface PeerInfo {
  id: string;
  address: string;
  direction: "incoming" | "outgoing";
  state: "connecting" | "connected" | "disconnected";
  disconnectReason?: "userRequested" | "networkError" | "remoteClosed" | "protocolError";
  connectionCount: number;
  connectedAt: Date;
  lastActivityAt: Date;
}

/**
 * Events emitted by PeerManager:
 * - 'peer-connected': New peer connected (ConnectedPeer)
 * - 'peer-disconnected': Peer disconnected (peerId: string)
 * - 'message': Message received from peer (peerId: string, data: Uint8Array)
 * - 'error': Error occurred (Error)
 */
export class PeerManager extends Events {
  private server: SyncWebSocketServer | null = null;
  private connections: Map<string, Connection> = new Map();
  private serverPort: number = DEFAULT_PORT;
  private ownPeerId: string;
  private pluginDir: string | null;
  private vault: VaultPeerManager | null = null;
  private advertisedAddress: string | null = null;

  /**
   * @param peerId - Our unique peer identifier
   * @param pluginDir - Absolute path to the plugin directory (for loading ws-server.js on desktop)
   */
  constructor(peerId: string, pluginDir: string | null = null) {
    super();
    this.ownPeerId = peerId;
    this.pluginDir = pluginDir;
  }

  /**
   * Set the vault for peer state management.
   * Must be called before connecting to peers.
   */
  setVault(vault: VaultPeerManager): void {
    this.vault = vault;
  }

  /**
   * Start the peer manager (starts WebSocket server on desktop).
   *
   * Returns the actual port the server is listening on (may differ from
   * requested port if it was in use).
   */
  async start(port: number = DEFAULT_PORT): Promise<number> {
    this.serverPort = port;

    // Only start server on desktop (mobile can't run a server)
    if (Platform.isDesktop && this.pluginDir) {
      // Load the WebSocket server module using Node's require with absolute path.
      // ws-server.js is built separately for Node.js platform (not bundled in main.js)
      // because the 'ws' package has browser stubs that break in Electron.
      const wsServerPath = `${this.pluginDir}/ws-server.js`;
      // eslint-disable-next-line @typescript-eslint/no-var-requires
      const { SyncWebSocketServer } = require(wsServerPath);
      const server: SyncWebSocketServer = new SyncWebSocketServer();
      this.server = server;

      server.on("connection", (conn: { id: string; remoteAddress: string }) => {
        // Store connection entry (server manages the socket)
        this.connections.set(conn.id, { socket: null });

        // Notify Rust of connecting state
        if (this.vault) {
          this.vault.peerConnecting(conn.id, conn.remoteAddress, "incoming");
        }

        // Send our peer ID as handshake
        this.sendHandshake(conn.id, "server");
      });

      server.on("message", (peerId: string, data: Uint8Array) => {
        this.handleMessage(peerId, data);
      });

      server.on("close", (connectionId: string) => {
        this.cleanupConnection(connectionId, "remoteClosed");
      });

      server.on("error", (err) => {
        this.trigger("error", err);
      });

      // Start server, retrying on port conflicts
      this.serverPort = await server.start({ port, maxRetries: 10 });
    }

    return this.serverPort;
  }

  /**
   * Stop the peer manager (stops server and closes all connections).
   */
  async stop(): Promise<void> {
    // Close all outgoing connections
    for (const [, conn] of this.connections) {
      if (conn.socket) {
        conn.socket.disconnect();
      }
    }
    this.connections.clear();

    // Stop server
    if (this.server) {
      await this.server.stop();
      this.server = null;
    }
  }

  /**
   * Connect to a peer at the given address.
   *
   * @param address - IP address or hostname
   * @param port - Port number (defaults to 8765)
   */
  async connectToPeer(address: string, port: number = DEFAULT_PORT): Promise<string> {
    const url = `ws://${address}:${port}`;
    const connectionId = `client-${address}:${port}`;

    if (this.connections.has(connectionId)) {
      throw new Error(`Already connected to ${address}:${port}`);
    }

    const client = new SyncWebSocketClient();

    // Add connection entry BEFORE connecting so it's available when 'open' fires
    this.connections.set(connectionId, { socket: client });
    this.setupOutgoingHandlers(connectionId, `${address}:${port}`, client, "networkError");

    await client.connect({ url, reconnect: true, reconnectDelay: 5000 });

    return connectionId;
  }

  /**
   * Normalize a WebSocket URL for consistent peer ID generation.
   * Removes default ports, query strings, and fragments.
   */
  private normalizeUrl(url: string): string {
    const parsed = new URL(url);
    // Remove default ports
    if (
      (parsed.protocol === "wss:" && parsed.port === "443") ||
      (parsed.protocol === "ws:" && parsed.port === "80")
    ) {
      parsed.port = "";
    }
    parsed.search = "";
    parsed.hash = "";
    return parsed.href.replace(/\/$/, "");
  }

  /**
   * Connect to a peer using a full WebSocket URL.
   * Use this for connecting through reverse proxies or with custom paths.
   *
   * @param url - Full WebSocket URL (e.g., wss://example.com/sync)
   */
  async connectToUrl(url: string, options?: { reconnect?: boolean }): Promise<string> {
    let normalized: string;
    try {
      normalized = this.normalizeUrl(url);
    } catch {
      throw new Error(`Invalid URL: ${url}`);
    }

    const connectionId = `url-${normalized}`;

    if (this.connections.has(connectionId)) {
      throw new Error(`Already connected to ${url}`);
    }

    const client = new SyncWebSocketClient();

    // Add connection entry BEFORE connecting so it's available when 'open' fires
    this.connections.set(connectionId, { socket: client });
    this.setupOutgoingHandlers(connectionId, normalized, client, "networkError");

    const reconnect = options?.reconnect ?? true;
    await client.connect({ url: normalized, reconnect, reconnectDelay: 5000 });

    return connectionId;
  }

  /**
   * Disconnect from a specific peer.
   */
  disconnectPeer(peerId: string): void {
    const { conn, connectionId } = this.resolveConnection(peerId);

    if (conn?.socket) {
      conn.socket.disconnect();
    } else if (this.server) {
      this.server.disconnect(connectionId);
    }

    this.cleanupConnection(connectionId, "userRequested");
  }

  /**
   * Send data to a specific peer.
   */
  send(peerId: string, data: Uint8Array): void {
    const { conn, connectionId } = this.resolveConnection(peerId);

    // Try outgoing connection
    if (conn?.socket?.isConnected) {
      conn.socket.send(data);
      return;
    }

    // Try incoming connection via server
    if (this.server) {
      try {
        this.server.send(connectionId, data);
        return;
      } catch {
        // Also try the original peerId in case it's the connection ID
        if (connectionId !== peerId) {
          try {
            this.server.send(peerId, data);
            return;
          } catch {
            // Peer not found
          }
        }
      }
    }

    throw new Error(`Peer not found: ${peerId}`);
  }

  /**
   * Broadcast data to all connected peers.
   */
  broadcast(data: Uint8Array): void {
    // Send to all outgoing connections. Connections map has dual-key entries
    // (connectionId + peerId) pointing to the same socket, so deduplicate.
    const sent = new Set<SyncWebSocketClient>();
    for (const conn of this.connections.values()) {
      if (conn.socket?.isConnected && !sent.has(conn.socket)) {
        conn.socket.send(data);
        sent.add(conn.socket);
      }
    }

    // Send to all incoming connections
    if (this.server) {
      this.server.broadcast(data);
    }
  }

  /**
   * Broadcast data to all connected peers except the specified one.
   */
  broadcastExcept(data: Uint8Array, excludePeerId: string): void {
    if (!this.vault) return;
    for (const peer of this.vault.getConnectedPeers()) {
      if (peer.id !== excludePeerId) {
        try {
          this.send(peer.id, data);
        } catch {
          // Peer may have disconnected between iteration and send
        }
      }
    }
  }

  /**
   * Get list of connected peers from Rust state.
   */
  getConnectedPeers(): PeerInfo[] {
    if (!this.vault) return [];
    return this.vault.getConnectedPeers().map((peer) => ({
      id: peer.id,
      address: peer.address,
      direction: peer.direction,
      state: peer.state,
      disconnectReason: peer.disconnectReason,
      connectionCount: peer.connectionCount,
      connectedAt: new Date(peer.firstSeen),
      lastActivityAt: new Date(peer.lastSeen),
    }));
  }

  /**
   * Update the last activity timestamp for a peer and emit event.
   */
  updatePeerActivity(peerId: string): void {
    // Activity is now tracked in Rust via touch() if needed
    this.trigger("peer-activity", peerId);
  }

  /**
   * Get the number of connected peers.
   */
  get peerCount(): number {
    if (!this.vault) return 0;
    return this.vault.getConnectedPeers().length;
  }

  /**
   * Get the server port.
   */
  get port(): number {
    return this.server?.port ?? this.serverPort;
  }

  /**
   * Check if server is running.
   */
  get isServerRunning(): boolean {
    return this.server?.isRunning ?? false;
  }

  /**
   * Get local network IP addresses for LAN connections.
   * Returns IPv4 addresses from non-internal network interfaces.
   */
  getLanAddresses(): string[] {
    if (!Platform.isDesktop) return [];

    try {
      // eslint-disable-next-line @typescript-eslint/no-var-requires
      const os = require("os");
      const interfaces = os.networkInterfaces();
      const addresses: string[] = [];

      for (const name of Object.keys(interfaces)) {
        for (const iface of interfaces[name]) {
          // Skip internal (loopback) and non-IPv4 addresses
          if (iface.internal || iface.family !== "IPv4") continue;
          addresses.push(iface.address);
        }
      }

      return addresses;
    } catch {
      return [];
    }
  }

  /**
   * Set our advertised address (used in handshake for peer discovery).
   */
  setAdvertisedAddress(address: string): void {
    this.advertisedAddress = address;
  }

  // ========== Private Helpers ==========

  /**
   * Clean up a connection and notify vault of the disconnect.
   */
  private cleanupConnection(connectionId: string, reason: DisconnectReason): void {
    const conn = this.connections.get(connectionId);
    const peerId = conn?.peerId ?? connectionId;

    this.connections.delete(connectionId);
    if (conn?.peerId && conn.peerId !== connectionId) {
      this.connections.delete(conn.peerId);
    }

    if (this.vault) {
      this.vault.peerDisconnected(peerId, reason);
    }

    this.trigger("peer-disconnected", peerId);
  }

  /**
   * Wire up open/message/close/error handlers for an outgoing client connection.
   */
  private setupOutgoingHandlers(
    connectionId: string,
    address: string,
    client: SyncWebSocketClient,
    reason: DisconnectReason
  ): void {
    client.on("open", () => {
      // Re-add connection on reconnect (client persists across reconnects)
      this.connections.set(connectionId, { socket: client });

      // Notify Rust of connecting state
      if (this.vault) {
        this.vault.peerConnecting(connectionId, address, "outgoing");
      }

      // Send handshake with error handling to prevent silent failures
      try {
        this.sendHandshake(connectionId, "client");
      } catch (err) {
        log.error(`Failed to send handshake to ${connectionId}:`, err);
        this.trigger("error", err);
      }
    });

    client.on("message", (data) => {
      this.handleMessage(connectionId, data as Uint8Array);
    });

    client.on("close", () => {
      this.cleanupConnection(connectionId, reason);
    });

    client.on("error", (err) => {
      this.trigger("error", err);
    });
  }

  /**
   * Resolve a peer ID to its connection entry, checking vault for mapping.
   */
  private resolveConnection(peerId: string): { conn: Connection | undefined; connectionId: string } {
    let conn = this.connections.get(peerId);
    let connectionId = peerId;

    if (!conn && this.vault) {
      connectionId = this.vault.resolvePeerId(peerId);
      conn = this.connections.get(connectionId);
    }

    return { conn, connectionId };
  }

  /**
   * Send a handshake message with our peer ID.
   *
   * Wire format matches `sync_core::protocol::Handshake` in Rust:
   * ```json
   * { "type": "handshake", "peerId": "abc-123", "role": "client" }
   * ```
   */
  private sendHandshake(connectionId: string, role: "server" | "client"): void {
    const handshake: Record<string, string> = {
      type: "handshake",
      peerId: this.ownPeerId,
      role,
    };
    if (this.advertisedAddress) {
      handshake.address = this.advertisedAddress;
    }
    const data = new TextEncoder().encode(JSON.stringify(handshake));
    this.send(connectionId, data);
  }

  /**
   * Handle an incoming message.
   */
  private handleMessage(connectionId: string, data: Uint8Array): void {
    // Try to parse as JSON handshake first
    try {
      const text = new TextDecoder().decode(data);
      const msg = JSON.parse(text);

      if (msg.type === "handshake") {
        log.debug(`Received handshake from ${msg.peerId} (${msg.role})`);

        // Cache the real peer ID locally for message routing
        const conn = this.connections.get(connectionId);
        if (conn) {
          conn.peerId = msg.peerId;
          // Also index by real peer ID for lookups
          this.connections.set(msg.peerId, conn);
        }

        // Notify Rust of handshake completion
        let peer: ConnectedPeer | undefined;
        if (this.vault) {
          peer = this.vault.peerHandshakeComplete(connectionId, msg.peerId);
        }

        // Emit peer-connected event (now fired after handshake, not on socket open)
        if (peer) {
          this.trigger("peer-connected", peer);
        }
        return;
      }
    } catch {
      // Not JSON, treat as binary sync message
    }

    // Resolve the peer ID using cached value
    const conn = this.connections.get(connectionId);
    const resolvedPeerId = conn?.peerId ?? connectionId;

    // Forward to listeners (sync engine)
    this.trigger("message", resolvedPeerId, data);
  }
}
