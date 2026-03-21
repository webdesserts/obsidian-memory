/**
 * Registry of authenticated relay clients.
 *
 * Tracks connected clients by EndpointId and handles datagram forwarding between
 * them. The registry also manages peer-awareness bookkeeping: when a client
 * disconnects, all peers that received datagrams from it are notified via an
 * `EndpointGone` frame so they can clean up their connection state.
 */

import { WebSocket } from "ws";
import { FrameType, serializeFrame } from "./frames.js";
import type { EndpointId } from "./handshake.js";
import { endpointIdToBytes } from "./handshake.js";

interface RelayClient {
  endpointId: EndpointId;
  ws: WebSocket;
  /** EndpointIds that have received at least one datagram forwarded *from* this client. */
  sentTo: Set<EndpointId>;
}

/**
 * Manages connected relay clients and routes datagrams between them.
 *
 * Thread safety: Node.js is single-threaded, so no locking is required.
 */
export class ClientRegistry {
  private clients: Map<EndpointId, RelayClient> = new Map();

  /**
   * Registers a newly authenticated client.
   *
   * If another client is already registered with the same EndpointId (e.g. the
   * peer reconnected before the old TCP connection timed out), the old WebSocket
   * is closed before the new registration takes effect.
   */
  register(endpointId: EndpointId, ws: WebSocket): void {
    const existing = this.clients.get(endpointId);
    if (existing) {
      existing.ws.close(1008, "Replaced by newer connection");
    }
    this.clients.set(endpointId, { endpointId, ws, sentTo: new Set() });
  }

  /**
   * Removes a client from the registry and notifies every peer that received
   * traffic from it by sending an `EndpointGone` frame.
   */
  unregister(endpointId: EndpointId): void {
    const client = this.clients.get(endpointId);
    if (!client) return;
    this.clients.delete(endpointId);

    const goneFrame = serializeFrame({
      type: FrameType.EndpointGone,
      endpointKey: endpointIdToBytes(endpointId),
    });

    for (const peerId of client.sentTo) {
      const peer = this.clients.get(peerId);
      if (peer && peer.ws.readyState === WebSocket.OPEN) {
        peer.ws.send(goneFrame);
      }
    }
  }

  /**
   * Forwards a single datagram from `fromId` to `toId`.
   * Returns `false` if the destination is not connected.
   */
  forward(fromId: EndpointId, toId: EndpointId, ecn: number, payload: Uint8Array): boolean {
    const dest = this.clients.get(toId);
    if (!dest || dest.ws.readyState !== WebSocket.OPEN) return false;

    const frame = serializeFrame({
      type: FrameType.RelayToClientDatagram,
      srcEndpointId: endpointIdToBytes(fromId),
      ecn,
      payload,
    });

    dest.ws.send(frame);
    this._trackSentTo(fromId, toId);
    return true;
  }

  /**
   * Forwards a datagram batch from `fromId` to `toId`.
   * Returns `false` if the destination is not connected.
   */
  forwardBatch(
    fromId: EndpointId,
    toId: EndpointId,
    ecn: number,
    segmentSize: number,
    payload: Uint8Array,
  ): boolean {
    const dest = this.clients.get(toId);
    if (!dest || dest.ws.readyState !== WebSocket.OPEN) return false;

    const frame = serializeFrame({
      type: FrameType.RelayToClientDatagramBatch,
      srcEndpointId: endpointIdToBytes(fromId),
      ecn,
      segmentSize,
      payload,
    });

    dest.ws.send(frame);
    this._trackSentTo(fromId, toId);
    return true;
  }

  /** Number of currently registered clients. */
  get size(): number {
    return this.clients.size;
  }

  private _trackSentTo(fromId: EndpointId, toId: EndpointId): void {
    const sender = this.clients.get(fromId);
    if (sender) sender.sentTo.add(toId);
  }
}
