/**
 * PeerManager - Coordinates WebSocket server and client connections.
 *
 * Manages the lifecycle of P2P connections:
 * - Starts a WebSocket server to accept incoming connections
 * - Connects to other peers as a client
 * - Routes messages between peers and the WASM sync engine
 */

import { EventEmitter } from "events";
import { Platform } from "obsidian";
import { SyncWebSocketClient } from "./WebSocketClient";
import { log } from "../logger";

// Type for dynamically loaded WebSocket server
interface SyncWebSocketServer extends EventEmitter {
  start(options: { port: number; maxRetries?: number }): Promise<number>;
  stop(): Promise<void>;
  send(peerId: string, data: Uint8Array): void;
  broadcast(data: Uint8Array): void;
  disconnect(peerId: string): void;
  readonly port: number;
  readonly isRunning: boolean;
}

/** Default port for the WebSocket server */
const DEFAULT_PORT = 8765;

/** Information about a connected peer */
export interface PeerInfo {
  id: string;
  address: string;
  direction: "incoming" | "outgoing";
  connectedAt: Date;
  lastActivityAt: Date;
}

/**
 * Events emitted by PeerManager:
 * - 'peer-connected': New peer connected (PeerInfo)
 * - 'peer-disconnected': Peer disconnected (peerId: string)
 * - 'message': Message received from peer (peerId: string, data: Uint8Array)
 * - 'error': Error occurred (Error)
 */
export class PeerManager extends EventEmitter {
  private server: SyncWebSocketServer | null = null;
  private outgoingConnections: Map<string, SyncWebSocketClient> = new Map();
  private peers: Map<string, PeerInfo> = new Map();
  private serverPort: number = DEFAULT_PORT;
  private ownPeerId: string;
  private pluginDir: string | null;

  /**
   * Maps temporary connection IDs to their real peer ID.
   * tempId -> realPeerId (e.g., "peer-1" -> "abc-123-def")
   */
  private tempToRealId: Map<string, string> = new Map();
  
  /**
   * Maps real peer IDs back to their temp connection ID.
   * realPeerId -> tempId (e.g., "abc-123-def" -> "peer-1")
   * Used for server.send() which only knows temp IDs.
   */
  private realToTempId: Map<string, string> = new Map();

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
        const now = new Date();
        const peerInfo: PeerInfo = {
          id: conn.id,
          address: conn.remoteAddress,
          direction: "incoming",
          connectedAt: now,
          lastActivityAt: now,
        };
        this.peers.set(conn.id, peerInfo);
        this.emit("peer-connected", peerInfo);

        // Send our peer ID as handshake
        this.sendHandshake(conn.id, "server");
      });

      server.on("message", (peerId: string, data: Uint8Array) => {
        this.handleMessage(peerId, data);
      });

      server.on("close", (tempPeerId: string) => {
        // Clean up both temp and real peer IDs
        const realPeerId = this.tempToRealId.get(tempPeerId) ?? tempPeerId;
        this.peers.delete(tempPeerId);
        this.peers.delete(realPeerId);
        this.tempToRealId.delete(tempPeerId);
        this.realToTempId.delete(realPeerId);
        this.emit("peer-disconnected", realPeerId);
      });

      server.on("error", (err) => {
        this.emit("error", err);
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
    for (const client of this.outgoingConnections.values()) {
      client.disconnect();
    }
    this.outgoingConnections.clear();

    // Stop server
    if (this.server) {
      await this.server.stop();
      this.server = null;
    }

    this.peers.clear();
  }

  /**
   * Connect to a peer at the given address.
   *
   * @param address - IP address or hostname
   * @param port - Port number (defaults to 8765)
   */
  async connectToPeer(address: string, port: number = DEFAULT_PORT): Promise<string> {
    const url = `ws://${address}:${port}`;
    const peerId = `client-${address}:${port}`;

    if (this.outgoingConnections.has(peerId)) {
      throw new Error(`Already connected to ${address}:${port}`);
    }

    const client = new SyncWebSocketClient();

    // Add client to map BEFORE connecting so it's available when 'open' fires
    this.outgoingConnections.set(peerId, client);

    client.on("open", () => {
      const now = new Date();
      const peerInfo: PeerInfo = {
        id: peerId,
        address: `${address}:${port}`,
        direction: "outgoing",
        connectedAt: now,
        lastActivityAt: now,
      };
      this.peers.set(peerId, peerInfo);
      this.emit("peer-connected", peerInfo);

      // Send our peer ID as handshake
      this.sendHandshake(peerId, "client");
    });

    client.on("message", (data) => {
      this.handleMessage(peerId, data);
    });

    client.on("close", () => {
      // Clean up both temp and real peer IDs
      const realPeerId = this.tempToRealId.get(peerId) ?? peerId;
      this.outgoingConnections.delete(peerId);
      this.outgoingConnections.delete(realPeerId);
      this.peers.delete(peerId);
      this.peers.delete(realPeerId);
      this.tempToRealId.delete(peerId);
      this.realToTempId.delete(realPeerId);
      this.emit("peer-disconnected", realPeerId);
    });

    client.on("error", (err) => {
      this.emit("error", err);
    });

    await client.connect({ url, reconnect: true, reconnectDelay: 5000 });

    return peerId;
  }

  /**
   * Normalize a WebSocket URL for consistent peer ID generation.
   * Removes default ports, query strings, and fragments.
   */
  private normalizeUrl(url: string): string {
    const parsed = new URL(url);
    // Remove default ports
    if ((parsed.protocol === 'wss:' && parsed.port === '443') ||
        (parsed.protocol === 'ws:' && parsed.port === '80')) {
      parsed.port = '';
    }
    parsed.search = '';
    parsed.hash = '';
    return parsed.href.replace(/\/$/, '');
  }

  /**
   * Connect to a peer using a full WebSocket URL.
   * Use this for connecting through reverse proxies or with custom paths.
   *
   * @param url - Full WebSocket URL (e.g., wss://example.com/sync)
   */
  async connectToUrl(url: string): Promise<string> {
    let normalized: string;
    try {
      normalized = this.normalizeUrl(url);
    } catch {
      throw new Error(`Invalid URL: ${url}`);
    }

    const peerId = `url-${normalized}`;

    if (this.outgoingConnections.has(peerId)) {
      throw new Error(`Already connected to ${url}`);
    }

    const client = new SyncWebSocketClient();

    // Add client to map BEFORE connecting so it's available when 'open' fires
    this.outgoingConnections.set(peerId, client);

    client.on("open", () => {
      const now = new Date();
      const peerInfo: PeerInfo = {
        id: peerId,
        address: normalized,
        direction: "outgoing",
        connectedAt: now,
        lastActivityAt: now,
      };
      this.peers.set(peerId, peerInfo);
      this.emit("peer-connected", peerInfo);

      // Send our peer ID as handshake
      this.sendHandshake(peerId, "client");
    });

    client.on("message", (data) => {
      this.handleMessage(peerId, data);
    });

    client.on("close", () => {
      // Clean up both temp and real peer IDs
      const realPeerId = this.tempToRealId.get(peerId) ?? peerId;
      this.outgoingConnections.delete(peerId);
      this.outgoingConnections.delete(realPeerId);
      this.peers.delete(peerId);
      this.peers.delete(realPeerId);
      this.tempToRealId.delete(peerId);
      this.realToTempId.delete(realPeerId);
      this.emit("peer-disconnected", realPeerId);
    });

    client.on("error", (err) => {
      this.emit("error", err);
    });

    await client.connect({ url: normalized, reconnect: true, reconnectDelay: 5000 });

    return peerId;
  }

  /**
   * Disconnect from a specific peer.
   */
  disconnectPeer(peerId: string): void {
    // Check outgoing connections
    const client = this.outgoingConnections.get(peerId);
    if (client) {
      client.disconnect();
      this.outgoingConnections.delete(peerId);
      this.peers.delete(peerId);
      // Clean up ID maps
      const tempId = this.realToTempId.get(peerId);
      if (tempId) {
        this.tempToRealId.delete(tempId);
        this.realToTempId.delete(peerId);
      }
      this.emit("peer-disconnected", peerId);
      return;
    }

    // Check incoming connections - server uses temp IDs
    if (this.server) {
      const tempId = this.realToTempId.get(peerId) ?? peerId;
      this.server.disconnect(tempId);
      this.peers.delete(peerId);
      // Clean up ID maps
      this.tempToRealId.delete(tempId);
      this.realToTempId.delete(peerId);
      this.emit("peer-disconnected", peerId);
    }
  }

  /**
   * Send data to a specific peer.
   */
  send(peerId: string, data: Uint8Array): void {
    // Try outgoing connection first (check both real ID and temp ID)
    let client = this.outgoingConnections.get(peerId);
    if (!client) {
      // peerId might be a temp ID, try to get real ID
      const realId = this.tempToRealId.get(peerId);
      if (realId) {
        client = this.outgoingConnections.get(realId);
      }
    }
    if (client && client.isConnected) {
      client.send(data);
      return;
    }

    // Try incoming connection via server
    if (this.server) {
      // Server uses temp IDs (peer-1, peer-2, etc.)
      // If we have a real peer ID, look up the temp ID
      const tempId = this.realToTempId.get(peerId) ?? peerId;
      try {
        this.server.send(tempId, data);
        return;
      } catch {
        // Also try the original peerId in case it's already a temp ID
        if (tempId !== peerId) {
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
    // Send to all outgoing connections
    for (const client of this.outgoingConnections.values()) {
      if (client.isConnected) {
        client.send(data);
      }
    }

    // Send to all incoming connections
    if (this.server) {
      this.server.broadcast(data);
    }
  }

  /**
   * Get list of connected peers.
   */
  getConnectedPeers(): PeerInfo[] {
    return Array.from(this.peers.values());
  }

  /**
   * Update the last activity timestamp for a peer and emit event.
   */
  updatePeerActivity(peerId: string): void {
    const peer = this.peers.get(peerId);
    if (peer) {
      peer.lastActivityAt = new Date();
      this.emit("peer-activity", peerId);
    }
  }

  /**
   * Get the number of connected peers.
   */
  get peerCount(): number {
    return this.peers.size;
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
   * Send a handshake message with our peer ID.
   */
  private sendHandshake(peerId: string, role: "server" | "client"): void {
    const handshake = JSON.stringify({
      type: "handshake",
      peerId: this.ownPeerId,
      role,
    });
    const data = new TextEncoder().encode(handshake);
    this.send(peerId, data);
  }

  /**
   * Handle an incoming message.
   */
  private handleMessage(tempPeerId: string, data: Uint8Array): void {
    // Try to parse as JSON handshake first
    try {
      const text = new TextDecoder().decode(data);
      const msg = JSON.parse(text);

      if (msg.type === "handshake") {
        log.debug(`Received handshake from ${msg.peerId} (${msg.role})`);
        // Update peer info with their real peer ID
        const peerInfo = this.peers.get(tempPeerId);
        if (peerInfo) {
          peerInfo.id = msg.peerId;

          // Remove temp ID entry to avoid duplicates in getConnectedPeers()
          this.peers.delete(tempPeerId);
          // Store under real ID only
          this.peers.set(msg.peerId, peerInfo);

          // Map both directions for ID resolution (needed for send/disconnect)
          this.tempToRealId.set(tempPeerId, msg.peerId);
          this.realToTempId.set(msg.peerId, tempPeerId);

          // Also update outgoing connections map if this was an outgoing connection
          const client = this.outgoingConnections.get(tempPeerId);
          if (client) {
            this.outgoingConnections.delete(tempPeerId);
            this.outgoingConnections.set(msg.peerId, client);
          }
        }
        return;
      }
    } catch {
      // Not JSON, treat as binary sync message
    }

    // Resolve the peer ID (might be aliased after handshake)
    const resolvedPeerId = this.tempToRealId.get(tempPeerId) ?? tempPeerId;
    
    // Forward to listeners (sync engine)
    this.emit("message", resolvedPeerId, data);
  }
}
