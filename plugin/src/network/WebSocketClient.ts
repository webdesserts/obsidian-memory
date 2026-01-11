/**
 * WebSocket client for connecting to peer servers.
 *
 * Uses the browser WebSocket API (available in Electron renderer).
 */

import { EventEmitter } from "events";
import { log } from "../logger";

export interface ClientOptions {
  url: string;
  reconnect?: boolean;
  reconnectDelay?: number;
}

/**
 * Events emitted by WebSocketClient:
 * - 'open': Connection established
 * - 'message': Message received (data: Uint8Array)
 * - 'close': Connection closed
 * - 'error': Connection error (Error)
 */
export class SyncWebSocketClient extends EventEmitter {
  private socket: WebSocket | null = null;
  private options: ClientOptions | null = null;
  private reconnecting = false;
  private shouldReconnect = false;

  /**
   * Connect to a peer server.
   */
  async connect(options: ClientOptions): Promise<void> {
    if (this.socket) {
      throw new Error("Already connected. Call disconnect() first.");
    }

    this.options = options;
    this.shouldReconnect = options.reconnect ?? false;

    return new Promise((resolve, reject) => {
      try {
        this.socket = new WebSocket(options.url);
        this.socket.binaryType = "arraybuffer";

        this.socket.onopen = () => {
          log.debug(`Connected to ${options.url}`);
          this.reconnecting = false;
          this.emit("open");
          resolve();
        };

        this.socket.onerror = (event) => {
          log.error("WebSocket error:", event);
          const err = new Error("WebSocket connection failed");
          this.emit("error", err);
          if (!this.reconnecting) {
            reject(err);
          }
        };

        this.socket.onmessage = (event) => {
          // Convert to Uint8Array
          let bytes: Uint8Array;
          if (event.data instanceof ArrayBuffer) {
            bytes = new Uint8Array(event.data);
          } else if (typeof event.data === "string") {
            // Text message - encode as UTF-8
            const encoder = new TextEncoder();
            bytes = encoder.encode(event.data);
          } else {
            log.warn("Unexpected message type:", typeof event.data);
            return;
          }
          this.emit("message", bytes);
        };

        this.socket.onclose = () => {
          log.debug(`Disconnected from ${options.url}`);
          this.socket = null;
          this.emit("close");

          // Attempt reconnect if enabled
          if (this.shouldReconnect && !this.reconnecting) {
            this.scheduleReconnect();
          }
        };
      } catch (err) {
        reject(err);
      }
    });
  }

  /**
   * Disconnect from the peer server.
   */
  disconnect(): void {
    this.shouldReconnect = false;

    if (this.socket) {
      this.socket.close();
      this.socket = null;
    }
  }

  /**
   * Send data to the connected peer.
   */
  send(data: Uint8Array): void {
    if (!this.socket || this.socket.readyState !== WebSocket.OPEN) {
      throw new Error("Not connected");
    }

    this.socket.send(data);
  }

  /**
   * Check if connected.
   */
  get isConnected(): boolean {
    return this.socket !== null && this.socket.readyState === WebSocket.OPEN;
  }

  /**
   * Get the connection URL.
   */
  get url(): string | null {
    return this.options?.url ?? null;
  }

  private scheduleReconnect(): void {
    if (this.reconnecting) return;

    this.reconnecting = true;
    const delay = this.options?.reconnectDelay ?? 3000;

    log.debug(`Reconnecting in ${delay}ms...`);

    setTimeout(async () => {
      if (this.shouldReconnect && this.options) {
        try {
          await this.connect(this.options);
        } catch {
          // Will retry on next close event
        }
      }
      this.reconnecting = false;
    }, delay);
  }
}
