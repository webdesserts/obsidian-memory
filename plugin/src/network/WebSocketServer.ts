/**
 * WebSocket server for accepting peer connections.
 *
 * Runs inside the Obsidian plugin using Node.js `ws` package.
 * Only available on desktop (Electron).
 */

import { WebSocketServer as WsServer, type WebSocket, type RawData } from "ws";
import type { IncomingMessage } from "http";
import { EventEmitter } from "events";
import { log } from "../logger";

export interface ServerOptions {
  port: number;
  host?: string;
}

export interface PeerConnection {
  id: string;
  socket: WebSocket;
  remoteAddress: string;
}

/**
 * Events emitted by WebSocketServer:
 * - 'connection': New peer connected (PeerConnection)
 * - 'message': Message received from peer (id: string, data: Uint8Array)
 * - 'close': Peer disconnected (id: string)
 * - 'error': Server error (Error)
 * - 'listening': Server started listening
 */
export class SyncWebSocketServer extends EventEmitter {
  private server: WsServer | null = null;
  private connections: Map<string, PeerConnection> = new Map();
  private nextConnectionId = 1;
  private _port: number = 0;

  /**
   * The port the server is listening on (0 if not running).
   */
  get port(): number {
    return this._port;
  }

  /**
   * Start the WebSocket server.
   * 
   * If the port is in use, will try up to maxRetries additional ports.
   */
  async start(options: ServerOptions & { maxRetries?: number }): Promise<number> {
    if (this.server) {
      throw new Error("Server already running");
    }

    const maxRetries = options.maxRetries ?? 5;
    let lastError: Error | null = null;

    for (let attempt = 0; attempt <= maxRetries; attempt++) {
      const port = options.port + attempt;
      try {
        await this.tryStart(port, options.host);
        return port;
      } catch (err: unknown) {
        lastError = err as Error;
        if ((err as NodeJS.ErrnoException).code === 'EADDRINUSE') {
          log.debug(`Port ${port} in use, trying next...`);
          continue;
        }
        throw err;
      }
    }

    throw lastError || new Error(`Failed to start server after ${maxRetries} retries`);
  }

  /**
   * Try to start the server on a specific port.
   */
  private tryStart(port: number, host?: string): Promise<void> {
    return new Promise((resolve, reject) => {
      const server = new WsServer({
        port,
        host: host || "0.0.0.0",
      });

      server.on("listening", () => {
        log.debug(`WebSocket server listening on port ${port}`);
        this.server = server;
        this._port = port;
        this.setupServerEvents(server);
        this.emit("listening", port);
        resolve();
      });

      server.on("error", (err: Error) => {
        server.close();
        reject(err);
      });
    });
  }

  /**
   * Set up event handlers for the server.
   */
  private setupServerEvents(server: WsServer): void {
    server.on("error", (err: Error) => {
      log.error("WebSocket server error:", err);
      this.emit("error", err);
    });

    server.on("connection", (socket: WebSocket, req: IncomingMessage) => {
      const id = `peer-${this.nextConnectionId++}`;
      const remoteAddress = req.socket.remoteAddress || "unknown";

      log.debug(`Peer connected: ${id} from ${remoteAddress}`);

      const connection: PeerConnection = {
        id,
        socket,
        remoteAddress,
      };

      this.connections.set(id, connection);
      this.emit("connection", connection);

      socket.on("message", (data: RawData) => {
        // Convert to Uint8Array for consistent handling
        let bytes: Uint8Array;
        if (data instanceof Buffer) {
          bytes = new Uint8Array(data);
        } else if (data instanceof ArrayBuffer) {
          bytes = new Uint8Array(data);
        } else if (Array.isArray(data)) {
          // Array of Buffers
          bytes = new Uint8Array(Buffer.concat(data));
        } else {
          bytes = new Uint8Array(data as Buffer);
        }
        this.emit("message", id, bytes);
      });

      socket.on("close", () => {
        log.debug(`Peer disconnected: ${id}`);
        this.connections.delete(id);
        this.emit("close", id);
      });

      socket.on("error", (err: Error) => {
        log.error(`Peer ${id} error:`, err);
      });
    });
  }

  /**
   * Stop the WebSocket server.
   */
  async stop(): Promise<void> {
    if (!this.server) {
      return;
    }

    return new Promise((resolve) => {
      // Close all connections
      for (const conn of this.connections.values()) {
        conn.socket.close();
      }
      this.connections.clear();

      this.server!.close(() => {
        log.debug("WebSocket server stopped");
        this.server = null;
        resolve();
      });
    });
  }

  /**
   * Send data to a specific peer.
   */
  send(peerId: string, data: Uint8Array): void {
    const conn = this.connections.get(peerId);
    if (!conn) {
      throw new Error(`Peer not found: ${peerId}`);
    }

    conn.socket.send(data);
  }

  /**
   * Send data to all connected peers.
   */
  broadcast(data: Uint8Array): void {
    for (const conn of this.connections.values()) {
      conn.socket.send(data);
    }
  }

  /**
   * Close a specific peer connection.
   */
  disconnect(peerId: string): void {
    const conn = this.connections.get(peerId);
    if (conn) {
      conn.socket.close();
      this.connections.delete(peerId);
    }
  }

  /**
   * Get the number of connected peers.
   */
  get connectionCount(): number {
    return this.connections.size;
  }

  /**
   * Check if server is running.
   */
  get isRunning(): boolean {
    return this.server !== null;
  }

  /**
   * Get list of connected peer IDs.
   */
  getConnectedPeers(): string[] {
    return Array.from(this.connections.keys());
  }
}
