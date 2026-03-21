import { describe, it, expect } from "vitest";
import { FrameType, serializeFrame, parseFrame } from "../src/frames.js";

// Fixed 32-byte endpoint ID used across tests.
const ENDPOINT_A = new Uint8Array(32).fill(0xaa);
const ENDPOINT_B = new Uint8Array(32).fill(0xbb);

function roundTrip(frame: Parameters<typeof serializeFrame>[0]) {
  const serialized = serializeFrame(frame);
  return parseFrame(serialized);
}

describe("Frame serialization round-trips", () => {
  it("ServerChallenge (type 0) — 16-byte nonce", () => {
    const nonce = crypto.getRandomValues(new Uint8Array(16));
    const frame = roundTrip({ type: FrameType.ServerChallenge, nonce });
    expect(frame.type).toBe(FrameType.ServerChallenge);
    if (frame.type !== FrameType.ServerChallenge) return;
    expect(frame.nonce).toEqual(nonce);
  });

  it("ClientAuth (type 1) — 32B public key + 64B signature", () => {
    const publicKey = new Uint8Array(32).fill(0x01);
    const signature = new Uint8Array(64).fill(0x02);
    const frame = roundTrip({ type: FrameType.ClientAuth, publicKey, signature });
    expect(frame.type).toBe(FrameType.ClientAuth);
    if (frame.type !== FrameType.ClientAuth) return;
    expect(frame.publicKey).toEqual(publicKey);
    expect(frame.signature).toEqual(signature);
  });

  it("ServerConfirmsAuth (type 2) — empty payload", () => {
    const frame = roundTrip({ type: FrameType.ServerConfirmsAuth });
    expect(frame.type).toBe(FrameType.ServerConfirmsAuth);
  });

  it("ServerDeniesAuth (type 3) — reason string", () => {
    const frame = roundTrip({ type: FrameType.ServerDeniesAuth, reason: "Bad signature" });
    expect(frame.type).toBe(FrameType.ServerDeniesAuth);
    if (frame.type !== FrameType.ServerDeniesAuth) return;
    expect(frame.reason).toBe("Bad signature");
  });

  it("ClientToRelayDatagram (type 4) — dest + ECN + payload", () => {
    const payload = new Uint8Array([0xde, 0xad, 0xbe, 0xef]);
    const frame = roundTrip({
      type: FrameType.ClientToRelayDatagram,
      destEndpointId: ENDPOINT_A,
      ecn: 2,
      payload,
    });
    expect(frame.type).toBe(FrameType.ClientToRelayDatagram);
    if (frame.type !== FrameType.ClientToRelayDatagram) return;
    expect(frame.destEndpointId).toEqual(ENDPOINT_A);
    expect(frame.ecn).toBe(2);
    expect(frame.payload).toEqual(payload);
  });

  it("ClientToRelayDatagramBatch (type 5) — preserves segment size", () => {
    const payload = new Uint8Array(100).fill(0x55);
    const frame = roundTrip({
      type: FrameType.ClientToRelayDatagramBatch,
      destEndpointId: ENDPOINT_B,
      ecn: 3,
      segmentSize: 50,
      payload,
    });
    expect(frame.type).toBe(FrameType.ClientToRelayDatagramBatch);
    if (frame.type !== FrameType.ClientToRelayDatagramBatch) return;
    expect(frame.destEndpointId).toEqual(ENDPOINT_B);
    expect(frame.ecn).toBe(3);
    expect(frame.segmentSize).toBe(50);
    expect(frame.payload).toEqual(payload);
  });

  it("RelayToClientDatagram (type 6) — source + ECN + payload", () => {
    const payload = new Uint8Array([0x11, 0x22]);
    const frame = roundTrip({
      type: FrameType.RelayToClientDatagram,
      srcEndpointId: ENDPOINT_A,
      ecn: 0,
      payload,
    });
    expect(frame.type).toBe(FrameType.RelayToClientDatagram);
    if (frame.type !== FrameType.RelayToClientDatagram) return;
    expect(frame.srcEndpointId).toEqual(ENDPOINT_A);
    expect(frame.ecn).toBe(0);
    expect(frame.payload).toEqual(payload);
  });

  it("RelayToClientDatagramBatch (type 7) — source + ECN + segment size", () => {
    const payload = new Uint8Array(200).fill(0x77);
    const frame = roundTrip({
      type: FrameType.RelayToClientDatagramBatch,
      srcEndpointId: ENDPOINT_B,
      ecn: 1,
      segmentSize: 100,
      payload,
    });
    expect(frame.type).toBe(FrameType.RelayToClientDatagramBatch);
    if (frame.type !== FrameType.RelayToClientDatagramBatch) return;
    expect(frame.srcEndpointId).toEqual(ENDPOINT_B);
    expect(frame.ecn).toBe(1);
    expect(frame.segmentSize).toBe(100);
    expect(frame.payload).toEqual(payload);
  });

  it("EndpointGone (type 8) — 32-byte endpoint key", () => {
    const endpointKey = new Uint8Array(32).fill(0xcc);
    const frame = roundTrip({ type: FrameType.EndpointGone, endpointKey });
    expect(frame.type).toBe(FrameType.EndpointGone);
    if (frame.type !== FrameType.EndpointGone) return;
    expect(frame.endpointKey).toEqual(endpointKey);
  });

  it("Ping (type 9) — 8-byte data", () => {
    const data = new Uint8Array([1, 2, 3, 4, 5, 6, 7, 8]);
    const frame = roundTrip({ type: FrameType.Ping, data });
    expect(frame.type).toBe(FrameType.Ping);
    if (frame.type !== FrameType.Ping) return;
    expect(frame.data).toEqual(data);
  });

  it("Pong (type 10) — 8-byte data", () => {
    const data = new Uint8Array([8, 7, 6, 5, 4, 3, 2, 1]);
    const frame = roundTrip({ type: FrameType.Pong, data });
    expect(frame.type).toBe(FrameType.Pong);
    if (frame.type !== FrameType.Pong) return;
    expect(frame.data).toEqual(data);
  });

  it("Health (type 11) — empty problem string means healthy", () => {
    const frame = roundTrip({ type: FrameType.Health, problem: "" });
    expect(frame.type).toBe(FrameType.Health);
    if (frame.type !== FrameType.Health) return;
    expect(frame.problem).toBe("");
  });

  it("Health (type 11) — non-empty problem string", () => {
    const frame = roundTrip({ type: FrameType.Health, problem: "High load" });
    expect(frame.type).toBe(FrameType.Health);
    if (frame.type !== FrameType.Health) return;
    expect(frame.problem).toBe("High load");
  });

  it("Restarting (type 12) — reconnectIn and tryFor (big-endian u32)", () => {
    const frame = roundTrip({ type: FrameType.Restarting, reconnectIn: 1000, tryFor: 5000 });
    expect(frame.type).toBe(FrameType.Restarting);
    if (frame.type !== FrameType.Restarting) return;
    expect(frame.reconnectIn).toBe(1000);
    expect(frame.tryFor).toBe(5000);
  });

  describe("ECN byte preservation", () => {
    it("preserves all 4 ECN values through ClientToRelayDatagram", () => {
      for (const ecn of [0, 1, 2, 3]) {
        const frame = roundTrip({
          type: FrameType.ClientToRelayDatagram,
          destEndpointId: ENDPOINT_A,
          ecn,
          payload: new Uint8Array([0x00]),
        });
        if (frame.type !== FrameType.ClientToRelayDatagram) continue;
        expect(frame.ecn).toBe(ecn);
      }
    });
  });

  describe("Unknown frame types", () => {
    it("returns an UnknownFrame for type code >= 13", () => {
      // Manually construct a message with type = 13 and a small payload.
      const buf = new Uint8Array([13, 0xca, 0xfe]);
      const frame = parseFrame(buf);
      expect(frame.type).toBe("unknown");
      if (frame.type !== "unknown") return;
      expect(frame.frameType).toBe(13);
      expect(frame.payload).toEqual(new Uint8Array([0xca, 0xfe]));
    });

    it("round-trips an UnknownFrame through serialize/parse", () => {
      const unknown = { type: "unknown" as const, frameType: 99, payload: new Uint8Array([0xff]) };
      const serialized = serializeFrame(unknown);
      const parsed = parseFrame(serialized);
      expect(parsed.type).toBe("unknown");
      if (parsed.type !== "unknown") return;
      expect(parsed.frameType).toBe(99);
      expect(parsed.payload).toEqual(new Uint8Array([0xff]));
    });
  });
});
