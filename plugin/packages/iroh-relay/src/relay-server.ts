/**
 * HTTP + WebSocket relay server.
 *
 * Listens on any TCP port and accepts WebSocket upgrades at the `/relay` path.
 * After a challenge-response handshake, authenticated clients can exchange
 * datagrams through the relay. The server is protocol-compatible with the iroh
 * Rust implementation (iroh-relay crate).
 */

import * as http from "node:http";
import { WebSocketServer, WebSocket } from "ws";
import { ClientRegistry } from "./client-registry.js";
import {
  parseFrame,
  serializeFrame,
  FrameType,
  type RelayFrame,
  type ClientToRelayDatagramFrame,
  type ClientToRelayDatagramBatchFrame,
} from "./frames.js";
import {
  generateChallenge,
  verifyClientAuth,
  publicKeyToEndpointId,
  type EndpointId,
} from "./handshake.js";

/** Milliseconds before we give up waiting for a `ClientAuth` frame. */
const HANDSHAKE_TIMEOUT_MS = 5_000;
/** How often we send a server-initiated Ping (base interval). */
const PING_INTERVAL_MS = 15_000;
/** Maximum random jitter added to the ping interval. */
const PING_JITTER_MS = 5_000;
/** How long we wait for a Pong response before closing the connection. */
const PONG_TIMEOUT_MS = 30_000;

/** Outcome returned by `start()`. */
export interface ServerStartResult {
  port: number;
  url: string;
}

/**
 * An iroh-compatible WebSocket relay server.
 *
 * @example
 * const server = new IrohRelayServer();
 * const { port, url } = await server.start(0); // bind to a random port
 * console.log(`Relay listening at ${url}`);
 * await server.stop();
 */
export class IrohRelayServer {
  private httpServer: http.Server | null = null;
  private wss: WebSocketServer | null = null;
  private registry = new ClientRegistry();
  private pingIntervals: Map<EndpointId, ReturnType<typeof setInterval>> = new Map();
  private pongTimeouts: Map<EndpointId, ReturnType<typeof setTimeout>> = new Map();

  /**
   * Starts the HTTP server and begins accepting WebSocket connections.
   * Pass `port: 0` to bind to a random available port.
   */
  async start(port = 0): Promise<ServerStartResult> {
    return new Promise((resolve, reject) => {
      const server = http.createServer();
      this.httpServer = server;

      const wss = new WebSocketServer({
        noServer: true,
        handleProtocols: (protocols) => {
          // Negotiate the iroh relay subprotocol. If the client doesn't request it,
          // handleProtocols returning false causes ws to reject with 400.
          return protocols.has("iroh-relay-v1") ? "iroh-relay-v1" : false;
        },
      });
      this.wss = wss;

      server.on("upgrade", (req, socket, head) => {
        const url = new URL(req.url ?? "/", `http://${req.headers.host}`);

        // Reject upgrades on any path other than /relay.
        if (url.pathname !== "/relay") {
          socket.write("HTTP/1.1 404 Not Found\r\n\r\n");
          socket.destroy();
          return;
        }

        wss.handleUpgrade(req, socket, head, (ws) => {
          wss.emit("connection", ws, req);
        });
      });

      wss.on("connection", (ws) => {
        this._handleConnection(ws);
      });

      server.listen(port, () => {
        const addr = server.address();
        if (!addr || typeof addr === "string") {
          reject(new Error("Failed to bind server"));
          return;
        }
        const boundPort = addr.port;
        resolve({ port: boundPort, url: `ws://127.0.0.1:${boundPort}/relay` });
      });

      server.on("error", reject);
    });
  }

  /**
   * Sends a `Restarting` frame to all connected clients, then closes all
   * connections and stops the HTTP server.
   */
  async stop(): Promise<void> {
    const restartingFrame = serializeFrame({
      type: FrameType.Restarting,
      reconnectIn: 0,
      tryFor: 0,
    });

    // Notify all clients before tearing down.
    if (this.wss) {
      for (const ws of this.wss.clients) {
        if (ws.readyState === WebSocket.OPEN) {
          ws.send(restartingFrame);
          ws.close(1001, "Server restarting");
        }
      }
    }

    // Cancel all ping/pong timers.
    for (const timer of this.pingIntervals.values()) clearInterval(timer);
    for (const timer of this.pongTimeouts.values()) clearTimeout(timer);
    this.pingIntervals.clear();
    this.pongTimeouts.clear();

    return new Promise((resolve, reject) => {
      this.wss?.close(() => {
        this.httpServer?.close((err) => {
          if (err) reject(err);
          else resolve();
        });
      });
    });
  }

  /** Number of currently authenticated clients. */
  get clientCount(): number {
    return this.registry.size;
  }

  // ---- Private: per-connection logic ----

  private _handleConnection(ws: WebSocket): void {
    const challenge = generateChallenge();
    let endpointId: EndpointId | null = null;
    let authenticated = false;

    // Send the challenge immediately.
    ws.send(serializeFrame({ type: FrameType.ServerChallenge, nonce: challenge }));

    // The client must respond with ClientAuth within HANDSHAKE_TIMEOUT_MS.
    const handshakeTimer = setTimeout(() => {
      if (!authenticated) {
        ws.close(1008, "Handshake timeout");
      }
    }, HANDSHAKE_TIMEOUT_MS);

    ws.on("message", (data, isBinary) => {
      // Text frames are a protocol error.
      if (!isBinary) {
        ws.close(1003, "Protocol error: text frames not allowed");
        return;
      }

      const buf = data instanceof Buffer ? new Uint8Array(data) : (data as Uint8Array);
      let frame: RelayFrame;
      try {
        frame = parseFrame(buf);
      } catch {
        ws.close(1002, "Malformed frame");
        return;
      }

      if (!authenticated) {
        this._handleHandshakeFrame(ws, frame, challenge, handshakeTimer, (id) => {
          authenticated = true;
          endpointId = id;
          this._startPingLoop(id, ws);
        });
        return;
      }

      if (endpointId === null) return;
      this._handleRelayFrame(endpointId, ws, frame);
    });

    ws.on("close", () => {
      clearTimeout(handshakeTimer);
      if (endpointId) {
        this._stopPingLoop(endpointId);
        this.registry.unregister(endpointId);
      }
    });

    ws.on("error", () => {
      // The 'close' event always follows 'error', so cleanup happens there.
    });
  }

  private async _handleHandshakeFrame(
    ws: WebSocket,
    frame: RelayFrame,
    challenge: Uint8Array,
    handshakeTimer: ReturnType<typeof setTimeout>,
    onSuccess: (id: EndpointId) => void,
  ): Promise<void> {
    if (frame.type !== FrameType.ClientAuth) {
      ws.close(1008, "Expected ClientAuth frame");
      return;
    }

    const { publicKey, signature } = frame;
    const valid = await verifyClientAuth(challenge, publicKey, signature);

    if (!valid) {
      ws.send(serializeFrame({ type: FrameType.ServerDeniesAuth, reason: "Invalid signature" }));
      ws.close(1008, "Authentication failed");
      return;
    }

    clearTimeout(handshakeTimer);
    const id = publicKeyToEndpointId(publicKey);
    this.registry.register(id, ws);
    ws.send(serializeFrame({ type: FrameType.ServerConfirmsAuth }));
    onSuccess(id);
  }

  private _handleRelayFrame(endpointId: EndpointId, ws: WebSocket, frame: RelayFrame): void {
    switch (frame.type) {
      case FrameType.ClientToRelayDatagram:
        this._forwardDatagram(endpointId, frame);
        break;

      case FrameType.ClientToRelayDatagramBatch:
        this._forwardDatagramBatch(endpointId, frame);
        break;

      case FrameType.Ping:
        ws.send(serializeFrame({ type: FrameType.Pong, data: frame.data }));
        break;

      case FrameType.Pong:
        // Client responded to our server-initiated ping; cancel the timeout.
        this._onPongReceived(endpointId);
        break;

      case "unknown":
        // Unknown frame types are a protocol violation. Log and disconnect.
        console.warn(
          `[iroh-relay] Unknown frame type ${frame.frameType} from ${endpointId} — closing connection`,
        );
        ws.close(1002, `Unknown frame type: ${frame.frameType}`);
        break;

      default:
        // Frames that are valid but unexpected from a client (e.g. server-only frames).
        ws.close(1002, "Unexpected frame type from client");
        break;
    }
  }

  private _forwardDatagram(fromId: EndpointId, frame: ClientToRelayDatagramFrame): void {
    const toId = Array.from(frame.destEndpointId)
      .map((b) => b.toString(16).padStart(2, "0"))
      .join("");
    this.registry.forward(fromId, toId, frame.ecn, frame.payload);
  }

  private _forwardDatagramBatch(fromId: EndpointId, frame: ClientToRelayDatagramBatchFrame): void {
    const toId = Array.from(frame.destEndpointId)
      .map((b) => b.toString(16).padStart(2, "0"))
      .join("");
    this.registry.forwardBatch(fromId, toId, frame.ecn, frame.segmentSize, frame.payload);
  }

  // ---- Private: keepalive ping loop ----

  private _startPingLoop(endpointId: EndpointId, ws: WebSocket): void {
    const schedule = (): void => {
      const jitter = Math.floor(Math.random() * PING_JITTER_MS);
      const interval = setInterval(() => {
        if (ws.readyState !== WebSocket.OPEN) {
          this._stopPingLoop(endpointId);
          return;
        }

        const pingData = crypto.getRandomValues(new Uint8Array(8));
        ws.send(serializeFrame({ type: FrameType.Ping, data: pingData }));

        // If we don't see a Pong within PONG_TIMEOUT_MS, close the connection.
        const pongTimeout = setTimeout(() => {
          ws.close(1001, "Pong timeout");
        }, PONG_TIMEOUT_MS);

        // Store so it can be cleared on Pong or disconnect.
        this.pongTimeouts.set(endpointId, pongTimeout);
      }, PING_INTERVAL_MS + jitter);

      this.pingIntervals.set(endpointId, interval);
    };

    schedule();
  }

  private _stopPingLoop(endpointId: EndpointId): void {
    const interval = this.pingIntervals.get(endpointId);
    if (interval) {
      clearInterval(interval);
      this.pingIntervals.delete(endpointId);
    }
    const timeout = this.pongTimeouts.get(endpointId);
    if (timeout) {
      clearTimeout(timeout);
      this.pongTimeouts.delete(endpointId);
    }
  }

  private _onPongReceived(endpointId: EndpointId): void {
    const timeout = this.pongTimeouts.get(endpointId);
    if (timeout) {
      clearTimeout(timeout);
      this.pongTimeouts.delete(endpointId);
    }
  }
}
