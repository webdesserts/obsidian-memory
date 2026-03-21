/**
 * @webdesserts/iroh-relay — TypeScript iroh-compatible relay server.
 *
 * Implements the iroh relay protocol (WebSocket-based datagram forwarding with
 * ed25519 challenge-response authentication). No Obsidian dependencies — this
 * package can run standalone in any Node.js environment.
 *
 * @see https://docs.rs/iroh-relay/latest/iroh_relay/
 */

// Populated in subsequent commits:
// - varint.ts: QUIC VarInt encoding/decoding (RFC 9000 Section 16)
// - frames.ts: 13 relay frame types (encode/decode)
// - handshake.ts: Challenge-response authentication
// - client-registry.ts: Connected client management + packet forwarding
// - relay-server.ts: HTTP + WebSocket server lifecycle
