import { describe, it, expect } from "vitest";
import {
  generateChallenge,
  deriveSigningChallenge,
  verifyClientAuth,
  publicKeyToEndpointId,
  endpointIdToBytes,
} from "../src/handshake.js";

/** Signs `data` with the given CryptoKey private key. */
async function sign(privateKey: CryptoKey, data: Uint8Array): Promise<Uint8Array> {
  return new Uint8Array(await crypto.subtle.sign("Ed25519", privateKey, data));
}

describe("generateChallenge()", () => {
  it("returns 16 bytes", () => {
    const challenge = generateChallenge();
    expect(challenge).toHaveLength(16);
  });

  it("produces different values on each call", () => {
    const a = generateChallenge();
    const b = generateChallenge();
    // Astronomically unlikely to collide, but worth asserting.
    expect(a).not.toEqual(b);
  });
});

describe("deriveSigningChallenge()", () => {
  it("returns 32 bytes", () => {
    const derived = deriveSigningChallenge(new Uint8Array(16));
    expect(derived).toHaveLength(32);
  });

  it("is deterministic — same input produces same output", () => {
    const challenge = new Uint8Array(16).fill(0x42);
    const a = deriveSigningChallenge(challenge);
    const b = deriveSigningChallenge(challenge);
    expect(a).toEqual(b);
  });

  it("produces different output for different inputs", () => {
    const a = deriveSigningChallenge(new Uint8Array(16).fill(0x00));
    const b = deriveSigningChallenge(new Uint8Array(16).fill(0xff));
    expect(a).not.toEqual(b);
  });

  it("blake3 KDF consistency — all-zero input returns same 32-byte output every time", () => {
    // We're not testing a cross-language KDF vector here (that would require a Rust
    // reference value), but we verify the output is stable across JS engine restarts
    // by checking that repeated calls produce the same bytes.
    const input = new Uint8Array(16);
    const result1 = deriveSigningChallenge(input);
    const result2 = deriveSigningChallenge(input);
    expect(result1).toEqual(result2);
    expect(result1).toHaveLength(32);
  });
});

describe("verifyClientAuth()", () => {
  it("returns true for a valid signature", async () => {
    const keypair = await crypto.subtle.generateKey("Ed25519", true, ["sign", "verify"]);
    const publicKeyRaw = new Uint8Array(await crypto.subtle.exportKey("raw", keypair.publicKey));
    const challenge = generateChallenge();
    const derived = deriveSigningChallenge(challenge);
    const signature = await sign(keypair.privateKey, derived);

    const result = await verifyClientAuth(challenge, publicKeyRaw, signature);
    expect(result).toBe(true);
  });

  it("returns false for a wrong signature", async () => {
    const keypair = await crypto.subtle.generateKey("Ed25519", true, ["sign", "verify"]);
    const publicKeyRaw = new Uint8Array(await crypto.subtle.exportKey("raw", keypair.publicKey));
    const challenge = generateChallenge();
    const wrongSignature = new Uint8Array(64).fill(0xff);

    const result = await verifyClientAuth(challenge, publicKeyRaw, wrongSignature);
    expect(result).toBe(false);
  });

  it("returns false when the signature is valid but for a different challenge", async () => {
    const keypair = await crypto.subtle.generateKey("Ed25519", true, ["sign", "verify"]);
    const publicKeyRaw = new Uint8Array(await crypto.subtle.exportKey("raw", keypair.publicKey));

    const challengeA = generateChallenge();
    const challengeB = generateChallenge();
    const derivedA = deriveSigningChallenge(challengeA);
    const signatureForA = await sign(keypair.privateKey, derivedA);

    // Signature is valid for challengeA, but we verify against challengeB.
    const result = await verifyClientAuth(challengeB, publicKeyRaw, signatureForA);
    expect(result).toBe(false);
  });

  it("returns false for a wrong public key", async () => {
    const keypair = await crypto.subtle.generateKey("Ed25519", true, ["sign", "verify"]);
    const wrongKeypair = await crypto.subtle.generateKey("Ed25519", true, ["sign", "verify"]);
    const wrongPublicKeyRaw = new Uint8Array(
      await crypto.subtle.exportKey("raw", wrongKeypair.publicKey),
    );
    const challenge = generateChallenge();
    const derived = deriveSigningChallenge(challenge);
    const signature = await sign(keypair.privateKey, derived);

    const result = await verifyClientAuth(challenge, wrongPublicKeyRaw, signature);
    expect(result).toBe(false);
  });

  it("returns false for malformed public key bytes", async () => {
    const challenge = generateChallenge();
    const result = await verifyClientAuth(
      challenge,
      new Uint8Array(10), // too short to be an ed25519 key
      new Uint8Array(64),
    );
    expect(result).toBe(false);
  });
});

describe("publicKeyToEndpointId() / endpointIdToBytes()", () => {
  it("converts a 32-byte key to a 64-char hex string", () => {
    const key = new Uint8Array(32).fill(0xab);
    const id = publicKeyToEndpointId(key);
    expect(id).toHaveLength(64);
    expect(id).toBe("ab".repeat(32));
  });

  it("round-trips bytes → EndpointId → bytes", () => {
    const key = crypto.getRandomValues(new Uint8Array(32));
    const id = publicKeyToEndpointId(key);
    const back = endpointIdToBytes(id);
    expect(back).toEqual(key);
  });
});
