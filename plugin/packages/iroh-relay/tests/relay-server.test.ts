import { describe, it, expect, beforeEach, afterEach } from "vitest";
import { WebSocket } from "ws";
import { IrohRelayServer } from "../src/relay-server.js";
import { parseFrame, serializeFrame, FrameType } from "../src/frames.js";
import { generateChallenge, deriveSigningChallenge, publicKeyToEndpointId } from "../src/handshake.js";

/** Generates an ed25519 keypair and returns raw public key bytes alongside the private key. */
async function generateKeypair() {
  const key = await crypto.subtle.generateKey("Ed25519", true, ["sign", "verify"]);
  const publicKeyRaw = new Uint8Array(await crypto.subtle.exportKey("raw", key.publicKey));
  return { publicKey: publicKeyRaw, privateKey: key.privateKey };
}

/** Signs `data` with the given private key. */
async function sign(privateKey: CryptoKey, data: Uint8Array): Promise<Uint8Array> {
  return new Uint8Array(await crypto.subtle.sign("Ed25519", privateKey, data));
}

/**
 * Connects a WebSocket to the relay server, completes the handshake, and
 * resolves once `ServerConfirmsAuth` is received.
 *
 * Returns the connected socket and a helper to read the next incoming frame.
 */
async function connectAuthenticated(
  url: string,
  keypair: Awaited<ReturnType<typeof generateKeypair>>,
): Promise<{
  ws: WebSocket;
  nextFrame: () => Promise<ReturnType<typeof parseFrame>>;
}> {
  return new Promise((resolve, reject) => {
    const ws = new WebSocket(url, "iroh-relay-v1");
    const frameQueue: Uint8Array[] = [];
    const waiters: ((frame: ReturnType<typeof parseFrame>) => void)[] = [];

    function enqueue(buf: Uint8Array) {
      const frame = parseFrame(buf);
      const waiter = waiters.shift();
      if (waiter) waiter(frame);
      else frameQueue.push(buf);
    }

    function nextFrame(): Promise<ReturnType<typeof parseFrame>> {
      if (frameQueue.length > 0) {
        return Promise.resolve(parseFrame(frameQueue.shift()!));
      }
      return new Promise((res) => waiters.push(res));
    }

    ws.on("message", (data) => {
      const buf = data instanceof Buffer ? new Uint8Array(data) : (data as Uint8Array);
      enqueue(buf);
    });

    ws.on("open", async () => {
      // Expect ServerChallenge
      const challengeFrame = await nextFrame();
      if (challengeFrame.type !== FrameType.ServerChallenge) {
        reject(new Error(`Expected ServerChallenge, got ${challengeFrame.type}`));
        return;
      }

      // Sign and respond with ClientAuth
      const derived = deriveSigningChallenge(challengeFrame.nonce);
      const signature = await sign(keypair.privateKey, derived);
      ws.send(
        serializeFrame({
          type: FrameType.ClientAuth,
          publicKey: keypair.publicKey,
          signature,
        }),
      );

      // Expect ServerConfirmsAuth
      const confirmFrame = await nextFrame();
      if (confirmFrame.type !== FrameType.ServerConfirmsAuth) {
        reject(new Error(`Expected ServerConfirmsAuth, got ${confirmFrame.type}`));
        return;
      }

      resolve({ ws, nextFrame });
    });

    ws.on("error", reject);
  });
}

describe("IrohRelayServer", () => {
  let server: IrohRelayServer;
  let url: string;

  beforeEach(async () => {
    server = new IrohRelayServer();
    const result = await server.start(0);
    url = result.url;
  });

  afterEach(async () => {
    await server.stop();
  });

  describe("start()", () => {
    it("binds to a port and returns a non-zero port number", async () => {
      const server2 = new IrohRelayServer();
      const result = await server2.start(0);
      expect(result.port).toBeGreaterThan(0);
      expect(result.url).toMatch(/^ws:\/\/127\.0\.0\.1:\d+\/relay$/);
      await server2.stop();
    });
  });

  describe("WebSocket upgrade", () => {
    it("rejects connections on a path other than /relay", async () => {
      const wrongUrl = url.replace("/relay", "/wrong");
      await expect(
        new Promise<void>((_, reject) => {
          const ws = new WebSocket(wrongUrl, "iroh-relay-v1");
          ws.on("error", reject);
          ws.on("open", () => reject(new Error("Should not have connected")));
        }),
      ).rejects.toBeTruthy();
    });

    it("rejects connections without the iroh-relay-v1 subprotocol", async () => {
      await expect(
        new Promise<void>((_, reject) => {
          // Connect without specifying a subprotocol.
          const ws = new WebSocket(url);
          ws.on("error", reject);
          ws.on("open", () => reject(new Error("Should not have connected")));
        }),
      ).rejects.toBeTruthy();
    });
  });

  describe("handshake", () => {
    it("authenticates a client with a real ed25519 keypair", async () => {
      const keypair = await generateKeypair();
      const { ws } = await connectAuthenticated(url, keypair);
      expect(server.clientCount).toBe(1);
      ws.close();
    });

    it("rejects a client that sends a wrong signature", async () => {
      const keypair = await generateKeypair();

      await new Promise<void>((resolve, reject) => {
        const ws = new WebSocket(url, "iroh-relay-v1");
        ws.on("message", async (data) => {
          const buf = data instanceof Buffer ? new Uint8Array(data) : (data as Uint8Array);
          const frame = parseFrame(buf);
          if (frame.type === FrameType.ServerChallenge) {
            // Send a deliberately wrong signature.
            ws.send(
              serializeFrame({
                type: FrameType.ClientAuth,
                publicKey: keypair.publicKey,
                signature: new Uint8Array(64).fill(0xff),
              }),
            );
          } else if (frame.type === FrameType.ServerDeniesAuth) {
            resolve();
            ws.close();
          } else {
            reject(new Error(`Unexpected frame: ${frame.type}`));
          }
        });
        ws.on("error", () => resolve()); // connection close counts as success here
      });
    });
  });

  describe("datagram forwarding", () => {
    it("forwards a datagram from client A to client B with correct source EndpointId and ECN", async () => {
      const keypairA = await generateKeypair();
      const keypairB = await generateKeypair();

      const { ws: wsA } = await connectAuthenticated(url, keypairA);
      const { ws: wsB, nextFrame } = await connectAuthenticated(url, keypairB);

      expect(server.clientCount).toBe(2);

      // Convert B's public key to an EndpointId byte array for the datagram header.
      const destEndpointId = keypairB.publicKey; // 32 bytes
      const testPayload = new Uint8Array([0xde, 0xad, 0xbe, 0xef]);

      wsA.send(
        serializeFrame({
          type: FrameType.ClientToRelayDatagram,
          destEndpointId,
          ecn: 2,
          payload: testPayload,
        }),
      );

      // B should receive a RelayToClientDatagram with A's EndpointId as source.
      const received = await nextFrame();
      expect(received.type).toBe(FrameType.RelayToClientDatagram);
      if (received.type !== FrameType.RelayToClientDatagram) return;

      const expectedSrc = publicKeyToEndpointId(keypairA.publicKey);
      const actualSrc = Array.from(received.srcEndpointId)
        .map((b) => b.toString(16).padStart(2, "0"))
        .join("");
      expect(actualSrc).toBe(expectedSrc);
      expect(received.ecn).toBe(2);
      expect(received.payload).toEqual(testPayload);

      wsA.close();
      wsB.close();
    });
  });

  describe("Ping / Pong", () => {
    it("responds to a client-initiated Ping with a matching Pong", async () => {
      const keypair = await generateKeypair();
      const { ws, nextFrame } = await connectAuthenticated(url, keypair);

      const pingData = new Uint8Array([1, 2, 3, 4, 5, 6, 7, 8]);
      ws.send(serializeFrame({ type: FrameType.Ping, data: pingData }));

      const pong = await nextFrame();
      expect(pong.type).toBe(FrameType.Pong);
      if (pong.type !== FrameType.Pong) return;
      expect(pong.data).toEqual(pingData);

      ws.close();
    });
  });

  describe("stop()", () => {
    it("sends a Restarting frame to connected clients before closing", async () => {
      const keypair = await generateKeypair();
      const { nextFrame } = await connectAuthenticated(url, keypair);

      const stopPromise = server.stop();

      const frame = await nextFrame();
      expect(frame.type).toBe(FrameType.Restarting);

      await stopPromise;
      // Prevent afterEach from calling stop() again on an already-stopped server.
      server = new IrohRelayServer();
      await server.start(0);
    });
  });
});
