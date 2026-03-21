/**
 * iroh relay protocol frame types and serialization.
 *
 * Each WebSocket message contains exactly one frame encoded as:
 *   [VarInt frame type][payload bytes]
 *
 * Encoding notes
 * --------------
 * Handshake frames (types 0–3) use postcard serialization in the iroh Rust implementation.
 * For fixed-size byte arrays ([u8; N]), postcard encodes them as raw bytes with no
 * length prefix — identical to raw binary. This applies to ServerChallenge, ClientAuth,
 * and ServerConfirmsAuth. However, ServerDeniesAuth contains a String, which postcard
 * encodes as varint(len) + utf8_bytes. We handle this explicitly.
 *
 * Relay frames (types 4–12) use raw binary encoding directly.
 */

import { encodeVarInt, decodeVarInt } from "./varint.js";

/** Numeric identifiers for all iroh relay frame types. */
export enum FrameType {
  ServerChallenge = 0,
  ClientAuth = 1,
  ServerConfirmsAuth = 2,
  ServerDeniesAuth = 3,
  ClientToRelayDatagram = 4,
  ClientToRelayDatagramBatch = 5,
  RelayToClientDatagram = 6,
  RelayToClientDatagramBatch = 7,
  EndpointGone = 8,
  Ping = 9,
  Pong = 10,
  Health = 11,
  Restarting = 12,
}

// ---- Frame payload interfaces ----

/** S→C: 16-byte random nonce for the auth challenge. */
export interface ServerChallengeFrame {
  type: FrameType.ServerChallenge;
  nonce: Uint8Array; // 16 bytes
}

/** C→S: Client's ed25519 public key + signature over the derived challenge. */
export interface ClientAuthFrame {
  type: FrameType.ClientAuth;
  publicKey: Uint8Array; // 32 bytes
  signature: Uint8Array; // 64 bytes
}

/** S→C: Authentication succeeded. Empty payload. */
export interface ServerConfirmsAuthFrame {
  type: FrameType.ServerConfirmsAuth;
}

/** S→C: Authentication failed. Human-readable reason string. */
export interface ServerDeniesAuthFrame {
  type: FrameType.ServerDeniesAuth;
  reason: string;
}

/** C→S: Single datagram addressed to another endpoint. */
export interface ClientToRelayDatagramFrame {
  type: FrameType.ClientToRelayDatagram;
  destEndpointId: Uint8Array; // 32 bytes
  ecn: number; // 1 byte
  payload: Uint8Array;
}

/** C→S: Batch of datagrams to one endpoint, segmented at a fixed segment size. */
export interface ClientToRelayDatagramBatchFrame {
  type: FrameType.ClientToRelayDatagramBatch;
  destEndpointId: Uint8Array; // 32 bytes
  ecn: number; // 1 byte
  segmentSize: number; // 2 bytes, big-endian
  payload: Uint8Array;
}

/** S→C: Datagram forwarded from a remote endpoint. */
export interface RelayToClientDatagramFrame {
  type: FrameType.RelayToClientDatagram;
  srcEndpointId: Uint8Array; // 32 bytes
  ecn: number; // 1 byte
  payload: Uint8Array;
}

/** S→C: Batch of datagrams from a remote endpoint. */
export interface RelayToClientDatagramBatchFrame {
  type: FrameType.RelayToClientDatagramBatch;
  srcEndpointId: Uint8Array; // 32 bytes
  ecn: number; // 1 byte
  segmentSize: number; // 2 bytes, big-endian
  payload: Uint8Array;
}

/** S→C: A previously connected endpoint disconnected. */
export interface EndpointGoneFrame {
  type: FrameType.EndpointGone;
  endpointKey: Uint8Array; // 32 bytes
}

/** Either direction: liveness probe. The 8-byte data must be echoed back in the Pong. */
export interface PingFrame {
  type: FrameType.Ping;
  data: Uint8Array; // 8 bytes
}

/** Either direction: liveness response to a Ping. */
export interface PongFrame {
  type: FrameType.Pong;
  data: Uint8Array; // 8 bytes
}

/** S→C: Optional problem description; empty string means "healthy". */
export interface HealthFrame {
  type: FrameType.Health;
  problem: string;
}

/** S→C: Server is about to restart; clients should reconnect. */
export interface RestartingFrame {
  type: FrameType.Restarting;
  /** How long (ms) before the server goes down. */
  reconnectIn: number; // 4 bytes, big-endian u32
  /** How long (ms) the server will retry listening before giving up. */
  tryFor: number; // 4 bytes, big-endian u32
}

/** Represents a frame with a type code >= 13 that this library doesn't recognize. */
export interface UnknownFrame {
  type: "unknown";
  frameType: number;
  payload: Uint8Array;
}

export type RelayFrame =
  | ServerChallengeFrame
  | ClientAuthFrame
  | ServerConfirmsAuthFrame
  | ServerDeniesAuthFrame
  | ClientToRelayDatagramFrame
  | ClientToRelayDatagramBatchFrame
  | RelayToClientDatagramFrame
  | RelayToClientDatagramBatchFrame
  | EndpointGoneFrame
  | PingFrame
  | PongFrame
  | HealthFrame
  | RestartingFrame
  | UnknownFrame;

// ---- Serialization ----

const encoder = new TextEncoder();
const decoder = new TextDecoder();

/**
 * Encodes a frame as a WebSocket message: `[VarInt type][payload]`.
 */
export function serializeFrame(frame: RelayFrame): Uint8Array {
  const [typeCode, payload] = encodePayload(frame);
  const typePrefix = encodeVarInt(typeCode);
  const out = new Uint8Array(typePrefix.length + payload.length);
  out.set(typePrefix, 0);
  out.set(payload, typePrefix.length);
  return out;
}

function encodePayload(frame: RelayFrame): [number, Uint8Array] {
  switch (frame.type) {
    case FrameType.ServerChallenge:
      return [FrameType.ServerChallenge, frame.nonce];

    case FrameType.ClientAuth: {
      const buf = new Uint8Array(96);
      buf.set(frame.publicKey, 0);
      buf.set(frame.signature, 32);
      return [FrameType.ClientAuth, buf];
    }

    case FrameType.ServerConfirmsAuth:
      return [FrameType.ServerConfirmsAuth, new Uint8Array(0)];

    case FrameType.ServerDeniesAuth: {
      // postcard encodes String as varint(len) + utf8_bytes
      const reasonBytes = encoder.encode(frame.reason);
      const lenPrefix = encodeVarInt(reasonBytes.length);
      const buf = new Uint8Array(lenPrefix.length + reasonBytes.length);
      buf.set(lenPrefix, 0);
      buf.set(reasonBytes, lenPrefix.length);
      return [FrameType.ServerDeniesAuth, buf];
    }

    case FrameType.ClientToRelayDatagram: {
      const buf = new Uint8Array(33 + frame.payload.length);
      buf.set(frame.destEndpointId, 0);
      buf[32] = frame.ecn;
      buf.set(frame.payload, 33);
      return [FrameType.ClientToRelayDatagram, buf];
    }

    case FrameType.ClientToRelayDatagramBatch: {
      const buf = new Uint8Array(35 + frame.payload.length);
      const view = new DataView(buf.buffer);
      buf.set(frame.destEndpointId, 0);
      buf[32] = frame.ecn;
      view.setUint16(33, frame.segmentSize, false);
      buf.set(frame.payload, 35);
      return [FrameType.ClientToRelayDatagramBatch, buf];
    }

    case FrameType.RelayToClientDatagram: {
      const buf = new Uint8Array(33 + frame.payload.length);
      buf.set(frame.srcEndpointId, 0);
      buf[32] = frame.ecn;
      buf.set(frame.payload, 33);
      return [FrameType.RelayToClientDatagram, buf];
    }

    case FrameType.RelayToClientDatagramBatch: {
      const buf = new Uint8Array(35 + frame.payload.length);
      const view = new DataView(buf.buffer);
      buf.set(frame.srcEndpointId, 0);
      buf[32] = frame.ecn;
      view.setUint16(33, frame.segmentSize, false);
      buf.set(frame.payload, 35);
      return [FrameType.RelayToClientDatagramBatch, buf];
    }

    case FrameType.EndpointGone:
      return [FrameType.EndpointGone, frame.endpointKey];

    case FrameType.Ping:
      return [FrameType.Ping, frame.data];

    case FrameType.Pong:
      return [FrameType.Pong, frame.data];

    case FrameType.Health:
      return [FrameType.Health, encoder.encode(frame.problem)];

    case FrameType.Restarting: {
      const buf = new Uint8Array(8);
      const view = new DataView(buf.buffer);
      view.setUint32(0, frame.reconnectIn, false);
      view.setUint32(4, frame.tryFor, false);
      return [FrameType.Restarting, buf];
    }

    case "unknown":
      return [frame.frameType, frame.payload];
  }
}

/**
 * Parses a frame from a raw WebSocket message buffer.
 * Returns an `UnknownFrame` for type codes >= 13 rather than throwing.
 */
export function parseFrame(buf: Uint8Array): RelayFrame {
  const { value: typeCode, bytesRead } = decodeVarInt(buf, 0);
  const payload = buf.subarray(bytesRead);
  return decodePayload(typeCode, payload);
}

function assertMinLength(payload: Uint8Array, min: number, frameName: string): void {
  if (payload.length < min) {
    throw new Error(`${frameName} frame too short: expected at least ${min} bytes, got ${payload.length}`);
  }
}

function decodePayload(typeCode: number, payload: Uint8Array): RelayFrame {
  switch (typeCode) {
    case FrameType.ServerChallenge:
      assertMinLength(payload, 16, "ServerChallenge");
      return { type: FrameType.ServerChallenge, nonce: payload.slice(0, 16) };

    case FrameType.ClientAuth:
      assertMinLength(payload, 96, "ClientAuth");
      return {
        type: FrameType.ClientAuth,
        publicKey: payload.slice(0, 32),
        signature: payload.slice(32, 96),
      };

    case FrameType.ServerConfirmsAuth:
      return { type: FrameType.ServerConfirmsAuth };

    case FrameType.ServerDeniesAuth: {
      // postcard encodes String as varint(len) + utf8_bytes
      if (payload.length === 0) {
        return { type: FrameType.ServerDeniesAuth, reason: "" };
      }
      const { value: reasonLen, bytesRead: lenBytes } = decodeVarInt(payload, 0);
      return {
        type: FrameType.ServerDeniesAuth,
        reason: decoder.decode(payload.subarray(lenBytes, lenBytes + reasonLen)),
      };
    }

    case FrameType.ClientToRelayDatagram:
      assertMinLength(payload, 33, "ClientToRelayDatagram");
      return {
        type: FrameType.ClientToRelayDatagram,
        destEndpointId: payload.slice(0, 32),
        ecn: payload[32],
        payload: payload.slice(33),
      };

    case FrameType.ClientToRelayDatagramBatch: {
      assertMinLength(payload, 35, "ClientToRelayDatagramBatch");
      const view = new DataView(payload.buffer, payload.byteOffset, payload.byteLength);
      return {
        type: FrameType.ClientToRelayDatagramBatch,
        destEndpointId: payload.slice(0, 32),
        ecn: payload[32],
        segmentSize: view.getUint16(33, false),
        payload: payload.slice(35),
      };
    }

    case FrameType.RelayToClientDatagram:
      assertMinLength(payload, 33, "RelayToClientDatagram");
      return {
        type: FrameType.RelayToClientDatagram,
        srcEndpointId: payload.slice(0, 32),
        ecn: payload[32],
        payload: payload.slice(33),
      };

    case FrameType.RelayToClientDatagramBatch: {
      assertMinLength(payload, 35, "RelayToClientDatagramBatch");
      const view = new DataView(payload.buffer, payload.byteOffset, payload.byteLength);
      return {
        type: FrameType.RelayToClientDatagramBatch,
        srcEndpointId: payload.slice(0, 32),
        ecn: payload[32],
        segmentSize: view.getUint16(33, false),
        payload: payload.slice(35),
      };
    }

    case FrameType.EndpointGone:
      assertMinLength(payload, 32, "EndpointGone");
      return { type: FrameType.EndpointGone, endpointKey: payload.slice(0, 32) };

    case FrameType.Ping:
      assertMinLength(payload, 8, "Ping");
      return { type: FrameType.Ping, data: payload.slice(0, 8) };

    case FrameType.Pong:
      assertMinLength(payload, 8, "Pong");
      return { type: FrameType.Pong, data: payload.slice(0, 8) };

    case FrameType.Health:
      return { type: FrameType.Health, problem: decoder.decode(payload) };

    case FrameType.Restarting: {
      assertMinLength(payload, 8, "Restarting");
      const view = new DataView(payload.buffer, payload.byteOffset, payload.byteLength);
      return {
        type: FrameType.Restarting,
        reconnectIn: view.getUint32(0, false),
        tryFor: view.getUint32(4, false),
      };
    }

    default:
      return { type: "unknown", frameType: typeCode, payload: payload.slice() };
  }
}
