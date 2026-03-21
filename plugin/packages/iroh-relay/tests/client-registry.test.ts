import { describe, it, expect, vi, beforeEach } from "vitest";
import { ClientRegistry } from "../src/client-registry.js";
import { parseFrame, FrameType } from "../src/frames.js";
import { publicKeyToEndpointId } from "../src/handshake.js";

/** Creates a minimal mock WebSocket with a recorded `send` buffer and `close` spy. */
function makeMockWs() {
  const sent: Uint8Array[] = [];
  const ws = {
    readyState: 1, // WebSocket.OPEN
    send(data: unknown) {
      sent.push(data instanceof Buffer ? new Uint8Array(data) : (data as Uint8Array));
    },
    close: vi.fn(),
    sentFrames() {
      return sent.map((buf) => parseFrame(buf));
    },
  };
  return ws;
}

const ID_A = publicKeyToEndpointId(new Uint8Array(32).fill(0x0a));
const ID_B = publicKeyToEndpointId(new Uint8Array(32).fill(0x0b));

describe("ClientRegistry", () => {
  let registry: ClientRegistry;

  beforeEach(() => {
    registry = new ClientRegistry();
  });

  describe("register()", () => {
    it("increases client count", () => {
      const ws = makeMockWs();
      registry.register(ID_A, ws as never);
      expect(registry.size).toBe(1);
    });

    it("closes the old WebSocket on duplicate registration", () => {
      const wsOld = makeMockWs();
      const wsNew = makeMockWs();
      registry.register(ID_A, wsOld as never);
      registry.register(ID_A, wsNew as never);
      expect(wsOld.close).toHaveBeenCalledWith(1008, "Replaced by newer connection");
      expect(registry.size).toBe(1);
    });
  });

  describe("forward()", () => {
    it("delivers a RelayToClientDatagram frame to the destination", () => {
      const wsA = makeMockWs();
      const wsB = makeMockWs();
      registry.register(ID_A, wsA as never);
      registry.register(ID_B, wsB as never);

      const payload = new Uint8Array([0xde, 0xad]);
      const delivered = registry.forward(ID_A, ID_B, 0, payload);

      expect(delivered).toBe(true);
      const frames = wsB.sentFrames();
      expect(frames).toHaveLength(1);
      const frame = frames[0];
      expect(frame.type).toBe(FrameType.RelayToClientDatagram);
      if (frame.type !== FrameType.RelayToClientDatagram) return;
      expect(frame.payload).toEqual(payload);
    });

    it("preserves the ECN byte through forwarding", () => {
      const wsA = makeMockWs();
      const wsB = makeMockWs();
      registry.register(ID_A, wsA as never);
      registry.register(ID_B, wsB as never);

      registry.forward(ID_A, ID_B, 3, new Uint8Array([0x01]));
      const frame = wsB.sentFrames()[0];
      if (frame.type !== FrameType.RelayToClientDatagram) throw new Error("wrong type");
      expect(frame.ecn).toBe(3);
    });

    it("returns false when the destination is not registered", () => {
      const wsA = makeMockWs();
      registry.register(ID_A, wsA as never);
      const result = registry.forward(ID_A, ID_B, 0, new Uint8Array([0x01]));
      expect(result).toBe(false);
    });

    it("returns false when the destination WebSocket is not OPEN", () => {
      const wsA = makeMockWs();
      const wsB = makeMockWs();
      wsB.readyState = 3; // WebSocket.CLOSED
      registry.register(ID_A, wsA as never);
      registry.register(ID_B, wsB as never);
      const result = registry.forward(ID_A, ID_B, 0, new Uint8Array([0x01]));
      expect(result).toBe(false);
    });
  });

  describe("forwardBatch()", () => {
    it("delivers a RelayToClientDatagramBatch frame with correct segment size", () => {
      const wsA = makeMockWs();
      const wsB = makeMockWs();
      registry.register(ID_A, wsA as never);
      registry.register(ID_B, wsB as never);

      const payload = new Uint8Array(100).fill(0x55);
      registry.forwardBatch(ID_A, ID_B, 2, 50, payload);

      const frame = wsB.sentFrames()[0];
      expect(frame.type).toBe(FrameType.RelayToClientDatagramBatch);
      if (frame.type !== FrameType.RelayToClientDatagramBatch) return;
      expect(frame.segmentSize).toBe(50);
      expect(frame.ecn).toBe(2);
      expect(frame.payload).toEqual(payload);
    });
  });

  describe("unregister()", () => {
    it("removes the client from the registry", () => {
      const ws = makeMockWs();
      registry.register(ID_A, ws as never);
      expect(registry.size).toBe(1);
      registry.unregister(ID_A);
      expect(registry.size).toBe(0);
    });

    it("sends EndpointGone to all peers that received datagrams from the disconnecting client", () => {
      const wsA = makeMockWs();
      const wsB = makeMockWs();
      registry.register(ID_A, wsA as never);
      registry.register(ID_B, wsB as never);

      // B forwards to A, marking A in B's sentTo set.
      registry.forward(ID_B, ID_A, 0, new Uint8Array([0x01]));
      // Now A disconnects.
      registry.unregister(ID_A);

      // B should NOT receive EndpointGone for A because A didn't send to B.
      // Instead we check: B sent to A, but A disconnected, so B should receive EndpointGone.
      // Let's set up the correct scenario: A sends to B, then A disconnects.
      const wsA2 = makeMockWs();
      const wsB2 = makeMockWs();
      registry.register(ID_A, wsA2 as never);
      registry.register(ID_B, wsB2 as never);
      registry.forward(ID_A, ID_B, 0, new Uint8Array([0x01])); // A → B
      registry.unregister(ID_A); // A disconnects

      // B should receive an EndpointGone frame for A.
      const frames = wsB2.sentFrames();
      // First frame is the datagram we forwarded, second should be EndpointGone.
      const goneFrames = frames.filter((f) => f.type === FrameType.EndpointGone);
      expect(goneFrames).toHaveLength(1);
    });

    it("does not send EndpointGone to clients that never received traffic from the disconnecting client", () => {
      const wsA = makeMockWs();
      const wsB = makeMockWs();
      const wsC = makeMockWs();
      const ID_C = publicKeyToEndpointId(new Uint8Array(32).fill(0x0c));
      registry.register(ID_A, wsA as never);
      registry.register(ID_B, wsB as never);
      registry.register(ID_C, wsC as never);

      // A sends only to B, not to C.
      registry.forward(ID_A, ID_B, 0, new Uint8Array([0x01]));
      registry.unregister(ID_A);

      const cFrames = wsC.sentFrames().filter((f) => f.type === FrameType.EndpointGone);
      expect(cFrames).toHaveLength(0);
    });

    it("is a no-op for an unregistered EndpointId", () => {
      expect(() => registry.unregister(ID_A)).not.toThrow();
    });
  });
});
