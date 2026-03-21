/**
 * @webdesserts/iroh-relay — TypeScript iroh-compatible relay server.
 *
 * Implements the iroh relay protocol (WebSocket-based datagram forwarding with
 * ed25519 challenge-response authentication). No Obsidian dependencies — this
 * package can run standalone in any Node.js environment.
 *
 * @see https://docs.rs/iroh-relay/latest/iroh_relay/
 */

export { IrohRelayServer } from "./relay-server.js";
export type { ServerStartResult } from "./relay-server.js";

export { FrameType, serializeFrame, parseFrame } from "./frames.js";
export type {
  RelayFrame,
  ServerChallengeFrame,
  ClientAuthFrame,
  ServerConfirmsAuthFrame,
  ServerDeniesAuthFrame,
  ClientToRelayDatagramFrame,
  ClientToRelayDatagramBatchFrame,
  RelayToClientDatagramFrame,
  RelayToClientDatagramBatchFrame,
  EndpointGoneFrame,
  PingFrame,
  PongFrame,
  HealthFrame,
  RestartingFrame,
  UnknownFrame,
} from "./frames.js";

export { encodeVarInt, decodeVarInt } from "./varint.js";
export type { VarIntDecodeResult } from "./varint.js";

export type { EndpointId } from "./handshake.js";
export {
  generateChallenge,
  deriveSigningChallenge,
  verifyClientAuth,
  publicKeyToEndpointId,
  endpointIdToBytes,
} from "./handshake.js";
