/**
 * Challenge-response authentication for the iroh relay handshake.
 *
 * Protocol:
 *   1. Server sends a 16-byte random `ServerChallenge` nonce.
 *   2. Client derives a 32-byte signing target using blake3 KDF over the nonce.
 *   3. Client signs the derived bytes with its ed25519 private key.
 *   4. Server verifies the signature against the client's claimed public key.
 *
 * The blake3 keyed derivation ensures that a valid signature can only be produced
 * by someone who knows the private key *and* received this specific challenge —
 * preventing replay attacks and cross-protocol signature misuse.
 */

import { blake3 } from "@noble/hashes/blake3";

/** Domain separator for the iroh relay handshake KDF, matching the Rust implementation. */
const CHALLENGE_DOMAIN = "iroh-relay handshake v1 challenge signature";

/**
 * A 64-character lowercase hex string representing a 32-byte ed25519 public key.
 * Used as the stable identifier for a connected relay client.
 */
export type EndpointId = string;

/** Generates a fresh 16-byte random challenge nonce. */
export function generateChallenge(): Uint8Array {
  const challenge = new Uint8Array(16);
  crypto.getRandomValues(challenge);
  return challenge;
}

/**
 * Derives the 32-byte value that the client must sign.
 *
 * Uses blake3 with a context string for domain separation rather than a keyed
 * hash, so the nonce is the input and CHALLENGE_DOMAIN is the derivation context.
 */
export function deriveSigningChallenge(challenge: Uint8Array): Uint8Array {
  return blake3(challenge, {
    context: new TextEncoder().encode(CHALLENGE_DOMAIN),
    dkLen: 32,
  });
}

/**
 * Verifies that `signature` is a valid ed25519 signature of `deriveSigningChallenge(challenge)`
 * under `publicKey`. Returns `false` for any cryptographic failure rather than throwing.
 */
export async function verifyClientAuth(
  challenge: Uint8Array,
  publicKey: Uint8Array,
  signature: Uint8Array,
): Promise<boolean> {
  const derived = deriveSigningChallenge(challenge);
  try {
    const key = await crypto.subtle.importKey("raw", publicKey, "Ed25519", false, ["verify"]);
    return await crypto.subtle.verify("Ed25519", key, signature, derived);
  } catch {
    // importKey throws if the key bytes are malformed.
    return false;
  }
}

/** Converts a 32-byte public key into an `EndpointId` hex string. */
export function publicKeyToEndpointId(publicKey: Uint8Array): EndpointId {
  return Array.from(publicKey)
    .map((b) => b.toString(16).padStart(2, "0"))
    .join("");
}

/** Converts an `EndpointId` hex string back into a 32-byte Uint8Array. */
export function endpointIdToBytes(id: EndpointId): Uint8Array {
  const bytes = new Uint8Array(32);
  for (let i = 0; i < 32; i++) {
    bytes[i] = parseInt(id.slice(i * 2, i * 2 + 2), 16);
  }
  return bytes;
}
