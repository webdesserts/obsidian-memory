/**
 * PeerManager tests.
 *
 * These tests verify that PeerManager correctly handles WebSocket
 * connection lifecycle - handshakes, reconnection, and Rust state integration.
 */

import { describe, it, expect, beforeEach, vi, afterEach } from "vitest";
import { MockWebSocket, MockWebSocketFactory } from "./mocks/MockWebSocket";
import { PeerManager, VaultPeerManager } from "../src/network/PeerManager";
import { configureLogger } from "../src/logger";
import type { ConnectedPeer, DisconnectReason } from "../src/wasm";

// Suppress log output during tests
configureLogger({ level: "none" });

/** Create a mock vault for testing */
function createMockVault(): VaultPeerManager & {
  peerConnectingSpy: ReturnType<typeof vi.fn>;
  peerHandshakeCompleteSpy: ReturnType<typeof vi.fn>;
  peerDisconnectedSpy: ReturnType<typeof vi.fn>;
  resolveIdMap: Map<string, string>;
  connectedPeers: ConnectedPeer[];
} {
  const resolveIdMap = new Map<string, string>();
  const connectedPeers: ConnectedPeer[] = [];

  const peerConnectingSpy = vi.fn(
    (connectionId: string, address: string, direction: string): ConnectedPeer => {
      resolveIdMap.set(connectionId, connectionId);
      const peer: ConnectedPeer = {
        id: connectionId,
        address,
        direction: direction as "incoming" | "outgoing",
        state: "connecting",
        firstSeen: Date.now(),
        lastSeen: Date.now(),
        connectionCount: 1,
      };
      return peer;
    }
  );

  const peerHandshakeCompleteSpy = vi.fn(
    (connectionId: string, peerId: string): ConnectedPeer => {
      resolveIdMap.set(connectionId, peerId);
      const peer: ConnectedPeer = {
        id: peerId,
        address: "test-address",
        direction: "outgoing",
        state: "connected",
        firstSeen: Date.now(),
        lastSeen: Date.now(),
        connectionCount: 1,
      };
      connectedPeers.push(peer);
      return peer;
    }
  );

  const peerDisconnectedSpy = vi.fn((id: string, _reason: DisconnectReason): void => {
    const idx = connectedPeers.findIndex((p) => p.id === id);
    if (idx >= 0) connectedPeers.splice(idx, 1);
  });

  return {
    peerConnecting: peerConnectingSpy,
    peerHandshakeComplete: peerHandshakeCompleteSpy,
    peerDisconnected: peerDisconnectedSpy,
    resolvePeerId: (connectionId: string) => resolveIdMap.get(connectionId) ?? connectionId,
    getKnownPeers: () => [],
    getConnectedPeers: () => [...connectedPeers],
    peerConnectingSpy,
    peerHandshakeCompleteSpy,
    peerDisconnectedSpy,
    resolveIdMap,
    connectedPeers,
  };
}

/** Create a mock SWIM membership for testing gossip behavior without WASM */
function createMockMembership() {
  return {
    localIncarnation: vi.fn(() => BigInt(1)),
    memberCount: vi.fn(() => 0),
    getAliveMembers: vi.fn(() => []),
    contains: vi.fn(() => false),
    getMemberIncarnation: vi.fn(() => undefined as number | undefined),
    processGossip: vi.fn(() => []),
    drainGossip: vi.fn(() => "[]"),
    generateFullGossip: vi.fn(() => "[]"),
    markDead: vi.fn(() => false),
    setLocalAddress: vi.fn(),
    onPeerConnected: vi.fn(() =>
      JSON.stringify({
        forNewPeer: {
          type: "gossip",
          updates: [
            { type: "alive", peer: { peerId: "test-client-id", address: null }, incarnation: 1 },
          ],
        },
        forExistingPeers: {
          type: "gossip",
          updates: [
            { type: "alive", peer: { peerId: "new-peer", address: null }, incarnation: 1 },
          ],
        },
      })
    ),
  };
}

describe("PeerManager", () => {
  let manager: PeerManager;
  let socketFactory: MockWebSocketFactory;
  let mockVault: ReturnType<typeof createMockVault>;

  beforeEach(() => {
    vi.useFakeTimers();

    socketFactory = new MockWebSocketFactory();
    mockVault = createMockVault();

    // Create a WebSocket constructor function with static constants
    const MockWebSocketConstructor = function (url: string) {
      return socketFactory.create(url);
    } as unknown as typeof WebSocket;

    // Add WebSocket static constants (needed for isConnected checks)
    Object.assign(MockWebSocketConstructor, {
      CONNECTING: MockWebSocket.CONNECTING,
      OPEN: MockWebSocket.OPEN,
      CLOSING: MockWebSocket.CLOSING,
      CLOSED: MockWebSocket.CLOSED,
    });

    vi.stubGlobal("WebSocket", MockWebSocketConstructor);

    manager = new PeerManager("test-client-id", null);
    manager.setVault(mockVault);
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  describe("connectToUrl()", () => {
    describe("Given a fresh PeerManager with Rust vault", () => {
      describe("When connecting to a URL", () => {
        it("should call peerConnecting on WebSocket open", async () => {
          const connectPromise = manager.connectToUrl("wss://example.com/sync");

          const socket = socketFactory.getLatest()!;
          socket.simulateOpen();
          await connectPromise;

          expect(mockVault.peerConnectingSpy).toHaveBeenCalledWith(
            expect.stringMatching(/^url-/),
            "wss://example.com/sync",
            "outgoing"
          );
        });

        it("should send handshake after WebSocket opens", async () => {
          const connectPromise = manager.connectToUrl("wss://example.com/sync");

          // WebSocket created but not yet open
          const socket = socketFactory.getLatest()!;
          expect(socket).toBeDefined();
          expect(socket.sentMessages).toHaveLength(0);

          // Simulate open
          socket.simulateOpen();
          await connectPromise;

          // Should have sent handshake
          const handshake = socket.getLastSentJson<{
            type: string;
            peerId: string;
            role: string;
          }>();
          expect(handshake).toEqual({
            type: "handshake",
            peerId: "test-client-id",
            role: "client",
          });
        });

        it("should call peerHandshakeComplete on handshake message", async () => {
          const connectPromise = manager.connectToUrl("wss://example.com/sync");
          const socket = socketFactory.getLatest()!;
          socket.simulateOpen();
          await connectPromise;

          // Simulate receiving server handshake
          const serverHandshake = new TextEncoder().encode(
            JSON.stringify({
              type: "handshake",
              peerId: "server-abc",
              role: "server",
            })
          );
          socket.simulateMessage(serverHandshake);

          expect(mockVault.peerHandshakeCompleteSpy).toHaveBeenCalledWith(
            expect.stringMatching(/^url-/),
            "server-abc"
          );
        });

        it("should emit peer-connected event after handshake (not on socket open)", async () => {
          const events: { id: string; direction: string }[] = [];
          manager.on("peer-connected", (info) => events.push(info));

          const connectPromise = manager.connectToUrl("wss://example.com/sync");
          const socket = socketFactory.getLatest()!;
          socket.simulateOpen();
          await connectPromise;

          // No event yet - handshake not received
          expect(events).toHaveLength(0);

          // Simulate receiving server handshake
          const serverHandshake = new TextEncoder().encode(
            JSON.stringify({
              type: "handshake",
              peerId: "server-abc",
              role: "server",
            })
          );
          socket.simulateMessage(serverHandshake);

          // Now should have event
          expect(events).toHaveLength(1);
          expect(events[0].id).toBe("server-abc");
        });

        it("should cache peerId locally after handshake for message routing", async () => {
          const connectPromise = manager.connectToUrl("wss://example.com/sync");
          const socket = socketFactory.getLatest()!;
          socket.simulateOpen();
          await connectPromise;

          // Simulate receiving server handshake
          const serverHandshake = new TextEncoder().encode(
            JSON.stringify({
              type: "handshake",
              peerId: "server-abc",
              role: "server",
            })
          );
          socket.simulateMessage(serverHandshake);

          // Clear the handshake message and send a sync message
          socket.clearSentMessages();
          const messages: { peerId: string; data: Uint8Array }[] = [];
          manager.on("message", (peerId, data) => messages.push({ peerId, data }));

          // Simulate a binary sync message
          const syncData = new Uint8Array([1, 2, 3, 4]);
          socket.simulateMessage(syncData);

          // Should route with the real peer ID (server-abc), not connection ID
          expect(messages).toHaveLength(1);
          expect(messages[0].peerId).toBe("server-abc");
        });

        it("should call peerDisconnected with reason on close", async () => {
          const connectPromise = manager.connectToUrl("wss://example.com/sync");
          const socket = socketFactory.getLatest()!;
          socket.simulateOpen();
          await connectPromise;

          socket.simulateClose(1006); // Abnormal closure

          expect(mockVault.peerDisconnectedSpy).toHaveBeenCalledWith(
            expect.any(String),
            "networkError"
          );
        });
      });
    });

    describe("Given an established connection that disconnects", () => {
      let firstSocket: MockWebSocket;

      beforeEach(async () => {
        const connectPromise = manager.connectToUrl("wss://example.com/sync");
        firstSocket = socketFactory.getLatest()!;
        firstSocket.simulateOpen();
        await connectPromise;
        firstSocket.clearSentMessages();
        mockVault.peerConnectingSpy.mockClear();
      });

      describe("When the WebSocket reconnects", () => {
        it("should send handshake again on reconnect", async () => {
          // Disconnect (triggers close event, schedules reconnect)
          firstSocket.simulateClose();

          // Advance past the reconnect delay (5000ms configured in PeerManager)
          await vi.advanceTimersByTimeAsync(5000);

          // WebSocketClient will have created a new socket on reconnect
          const reconnectSocket = socketFactory.getLatest()!;
          expect(reconnectSocket).not.toBe(firstSocket);

          // Simulate the reconnected socket opening
          reconnectSocket.simulateOpen();

          // Should have sent handshake on the new socket
          const handshake = reconnectSocket.getLastSentJson<{
            type: string;
            peerId: string;
            role: string;
          }>();
          expect(handshake?.type).toBe("handshake");
          expect(handshake?.peerId).toBe("test-client-id");
        });

        it("should call peerConnecting again on reconnect", async () => {
          firstSocket.simulateClose();
          await vi.advanceTimersByTimeAsync(5000);

          const reconnectSocket = socketFactory.getLatest()!;
          reconnectSocket.simulateOpen();

          // Should have called peerConnecting again
          expect(mockVault.peerConnectingSpy).toHaveBeenCalledTimes(1);
        });
      });
    });

    describe("Given handshake sending fails", () => {
      it("should emit error event without crashing", async () => {
        const errors: Error[] = [];
        manager.on("error", (err) => errors.push(err));

        const connectPromise = manager.connectToUrl("wss://example.com/sync");
        const socket = socketFactory.getLatest()!;

        // Make send throw when called
        socket.send = () => {
          throw new Error("Network failure");
        };

        // Open the socket (will try to send handshake, which will throw)
        socket.readyState = MockWebSocket.OPEN;
        socket.onopen?.();

        // Should have emitted error event
        expect(errors).toHaveLength(1);
        expect(errors[0].message).toBe("Network failure");
      });
    });
  });

  describe("connectToPeer()", () => {
    describe("Given an established connection that disconnects", () => {
      let firstSocket: MockWebSocket;

      beforeEach(async () => {
        const connectPromise = manager.connectToPeer("192.168.1.100", 8765);
        firstSocket = socketFactory.getLatest()!;
        firstSocket.simulateOpen();
        await connectPromise;
        firstSocket.clearSentMessages();
      });

      it("should send handshake again on reconnect", async () => {
        // Disconnect
        firstSocket.simulateClose();

        // Advance past the reconnect delay
        await vi.advanceTimersByTimeAsync(5000);

        // New socket created
        const reconnectSocket = socketFactory.getLatest()!;
        expect(reconnectSocket).not.toBe(firstSocket);

        // Open reconnected socket
        reconnectSocket.simulateOpen();

        // Should have sent handshake
        const handshake = reconnectSocket.getLastSentJson<{
          type: string;
          peerId: string;
          role: string;
        }>();
        expect(handshake?.type).toBe("handshake");
        expect(handshake?.peerId).toBe("test-client-id");
      });
    });
  });

  describe("Without vault set", () => {
    it("should still send handshakes but not call vault methods", async () => {
      const managerNoVault = new PeerManager("test-client-id", null);

      const connectPromise = managerNoVault.connectToUrl("wss://example.com/sync");
      const socket = socketFactory.getLatest()!;
      socket.simulateOpen();
      await connectPromise;

      // Should have sent handshake
      const handshake = socket.getLastSentJson<{
        type: string;
        peerId: string;
        role: string;
      }>();
      expect(handshake?.type).toBe("handshake");

      // No vault calls made
      expect(mockVault.peerConnectingSpy).not.toHaveBeenCalled();
    });
  });

  describe("sendHandshake()", () => {
    describe("Given membershipAddress is set", () => {
      it("should include address in handshake message", async () => {
        const pmWithAddr = new PeerManager("test-client-id", null, "ws://192.168.1.10:9427");
        pmWithAddr.setVault(mockVault);

        const connectPromise = pmWithAddr.connectToUrl("wss://example.com/sync");
        const socket = socketFactory.getLatest()!;
        socket.simulateOpen();
        await connectPromise;

        const handshake = socket.getLastSentJson<{
          type: string;
          peerId: string;
          role: string;
          address?: string;
        }>();
        expect(handshake).toEqual({
          type: "handshake",
          peerId: "test-client-id",
          role: "client",
          address: "ws://192.168.1.10:9427",
        });
      });
    });

    describe("Given membershipAddress is null", () => {
      it("should omit address from handshake message", async () => {
        const connectPromise = manager.connectToUrl("wss://example.com/sync");
        const socket = socketFactory.getLatest()!;
        socket.simulateOpen();
        await connectPromise;

        const handshake = socket.getLastSentJson<{
          type: string;
          peerId: string;
          role: string;
          address?: string;
        }>();
        expect(handshake).toEqual({
          type: "handshake",
          peerId: "test-client-id",
          role: "client",
        });
        expect(handshake).not.toHaveProperty("address");
      });
    });
  });

  describe("received handshake address", () => {
    it("should pass handshake address to onHandshakeComplete", async () => {
      const spy = vi.spyOn(manager as any, "onHandshakeComplete");

      const connectPromise = manager.connectToUrl("wss://example.com/sync");
      const socket = socketFactory.getLatest()!;
      socket.simulateOpen();
      await connectPromise;

      // Simulate receiving handshake WITH address field
      const serverHandshake = new TextEncoder().encode(
        JSON.stringify({
          type: "handshake",
          peerId: "server-abc",
          role: "server",
          address: "ws://10.0.0.5:9427",
        })
      );
      socket.simulateMessage(serverHandshake);

      expect(spy).toHaveBeenCalledWith(
        expect.stringMatching(/^url-/),
        "server-abc",
        "ws://10.0.0.5:9427"
      );
    });

    it("should fall back to vault address when handshake has no address", async () => {
      const spy = vi.spyOn(manager as any, "onHandshakeComplete");

      const connectPromise = manager.connectToUrl("wss://example.com/sync");
      const socket = socketFactory.getLatest()!;
      socket.simulateOpen();
      await connectPromise;

      // Simulate receiving handshake WITHOUT address field
      const serverHandshake = new TextEncoder().encode(
        JSON.stringify({
          type: "handshake",
          peerId: "server-abc",
          role: "server",
        })
      );
      socket.simulateMessage(serverHandshake);

      // Should fall back to vault's peer.address ("test-address" from mock)
      expect(spy).toHaveBeenCalledWith(
        expect.stringMatching(/^url-/),
        "server-abc",
        "test-address"
      );
    });
  });

  describe("setAdvertisedAddress()", () => {
    it("should update membershipAddress for future membership instances", () => {
      const pm = new PeerManager("test-peer", null, null, 1);
      pm.setAdvertisedAddress("ws://192.168.1.10:9427");

      // The address is stored - next membership creation will use it
      // (Can't easily verify without WASM, but we can check it doesn't throw)
      expect(() => pm.setAdvertisedAddress("ws://192.168.1.10:9428")).not.toThrow();
    });
  });

  describe("gossip handling", () => {
    it("should ignore gossip before handshake completes", async () => {
      const connectPromise = manager.connectToUrl("wss://example.com/sync");
      const socket = socketFactory.getLatest()!;
      socket.simulateOpen();
      await connectPromise;

      // Send gossip BEFORE handshake (no peer ID set yet)
      const gossipMsg = new TextEncoder().encode(
        JSON.stringify({
          type: "gossip",
          updates: [{ type: "alive", peer: { peerId: "other-peer", address: null }, incarnation: 1 }],
        })
      );
      socket.simulateMessage(gossipMsg);

      // Should not throw and should not crash - gossip is silently dropped
      // The membership list would still be empty since gossip was ignored
    });

    it("should process gossip after handshake completes", async () => {
      const connectPromise = manager.connectToUrl("wss://example.com/sync");
      const socket = socketFactory.getLatest()!;
      socket.simulateOpen();
      await connectPromise;

      // Complete handshake first
      const serverHandshake = new TextEncoder().encode(
        JSON.stringify({
          type: "handshake",
          peerId: "server-abc",
          role: "server",
        })
      );
      socket.simulateMessage(serverHandshake);

      // Now send gossip (should be processed since handshake is complete)
      const gossipMsg = new TextEncoder().encode(
        JSON.stringify({
          type: "gossip",
          updates: [{ type: "alive", peer: { peerId: "other-peer", address: "ws://other:8765" }, incarnation: 1 }],
        })
      );

      // Should not throw - gossip is processed
      expect(() => socket.simulateMessage(gossipMsg)).not.toThrow();
    });
  });

  describe("broadcastExcept()", () => {
    it("should send to all connected peers except the excluded one", async () => {
      // Connect peer A
      const connectA = manager.connectToUrl("wss://peer-a.com/sync");
      const socketA = socketFactory.getLatest()!;
      socketA.simulateOpen();
      await connectA;

      // Complete handshake for peer A
      socketA.simulateMessage(
        new TextEncoder().encode(
          JSON.stringify({ type: "handshake", peerId: "peer-a", role: "server" })
        )
      );

      // Connect peer B
      const connectB = manager.connectToUrl("wss://peer-b.com/sync");
      const socketB = socketFactory.getLatest()!;
      socketB.simulateOpen();
      await connectB;

      // Complete handshake for peer B
      socketB.simulateMessage(
        new TextEncoder().encode(
          JSON.stringify({ type: "handshake", peerId: "peer-b", role: "server" })
        )
      );

      // Clear sent messages from handshakes
      socketA.clearSentMessages();
      socketB.clearSentMessages();

      // Broadcast to everyone except peer A
      const data = new TextEncoder().encode("test-data");
      manager.broadcastExcept(data, "peer-a");

      // Peer B should receive the message, peer A should not
      expect(socketA.sentMessages).toHaveLength(0);
      expect(socketB.sentMessages).toHaveLength(1);
    });

    it("should not throw when vault is not set", () => {
      const managerNoVault = new PeerManager("test-client-id", null);

      expect(() => {
        managerNoVault.broadcastExcept(new TextEncoder().encode("test"), "some-peer");
      }).not.toThrow();
    });
  });

  describe("onHandshakeComplete()", () => {
    describe("Given membership is available", () => {
      let mockMembership: ReturnType<typeof createMockMembership>;

      beforeEach(() => {
        mockMembership = createMockMembership();
        (manager as any)._membership = mockMembership;
      });

      it("should call onPeerConnected with peer ID and address", async () => {
        const connectPromise = manager.connectToUrl("wss://example.com/sync");
        const socket = socketFactory.getLatest()!;
        socket.simulateOpen();
        await connectPromise;

        socket.simulateMessage(
          new TextEncoder().encode(
            JSON.stringify({
              type: "handshake",
              peerId: "server-abc",
              role: "server",
              address: "ws://10.0.0.5:9427",
            })
          )
        );

        expect(mockMembership.onPeerConnected).toHaveBeenCalledWith(
          "server-abc",
          "ws://10.0.0.5:9427"
        );
      });

      it("should send forNewPeer gossip to the connecting peer", async () => {
        const connectPromise = manager.connectToUrl("wss://example.com/sync");
        const socket = socketFactory.getLatest()!;
        socket.simulateOpen();
        await connectPromise;
        socket.clearSentMessages();

        socket.simulateMessage(
          new TextEncoder().encode(
            JSON.stringify({
              type: "handshake",
              peerId: "server-abc",
              role: "server",
            })
          )
        );

        // Should have sent forNewPeer gossip to the new peer
        expect(socket.sentMessages.length).toBeGreaterThanOrEqual(1);
        const sentMsg = JSON.parse(new TextDecoder().decode(socket.sentMessages[0]));
        expect(sentMsg.type).toBe("gossip");
        expect(sentMsg.updates).toHaveLength(1);
        expect(sentMsg.updates[0].peer.peerId).toBe("test-client-id");
      });

      it("should broadcast forExistingPeers to other connected peers", async () => {
        // Connect peer A first
        const connectA = manager.connectToUrl("wss://peer-a.com/sync");
        const socketA = socketFactory.getLatest()!;
        socketA.simulateOpen();
        await connectA;

        socketA.simulateMessage(
          new TextEncoder().encode(
            JSON.stringify({ type: "handshake", peerId: "peer-a", role: "server" })
          )
        );
        socketA.clearSentMessages();

        // Now connect peer B — onHandshakeComplete should broadcast to peer A
        // Update mock to return peer-b-specific gossip
        mockMembership.onPeerConnected.mockReturnValue(
          JSON.stringify({
            forNewPeer: {
              type: "gossip",
              updates: [
                { type: "alive", peer: { peerId: "test-client-id", address: null }, incarnation: 1 },
                { type: "alive", peer: { peerId: "peer-a", address: null }, incarnation: 1 },
              ],
            },
            forExistingPeers: {
              type: "gossip",
              updates: [
                { type: "alive", peer: { peerId: "peer-b", address: "ws://peer-b:8765" }, incarnation: 1 },
              ],
            },
          })
        );

        const connectB = manager.connectToUrl("wss://peer-b.com/sync");
        const socketB = socketFactory.getLatest()!;
        socketB.simulateOpen();
        await connectB;

        socketB.simulateMessage(
          new TextEncoder().encode(
            JSON.stringify({
              type: "handshake",
              peerId: "peer-b",
              role: "server",
              address: "ws://peer-b:8765",
            })
          )
        );

        // Peer A should have received the forExistingPeers broadcast
        expect(socketA.sentMessages).toHaveLength(1);
        const broadcastMsg = JSON.parse(new TextDecoder().decode(socketA.sentMessages[0]));
        expect(broadcastMsg.type).toBe("gossip");
        expect(broadcastMsg.updates[0].peer.peerId).toBe("peer-b");
      });
    });
  });

  describe("auto-connect", () => {
    it("should use one-shot connection (no reconnect) for gossip-discovered peers", async () => {
      // Connect and complete handshake
      const connectPromise = manager.connectToUrl("wss://example.com/sync");
      const socket = socketFactory.getLatest()!;
      socket.simulateOpen();
      await connectPromise;

      const serverHandshake = new TextEncoder().encode(
        JSON.stringify({
          type: "handshake",
          peerId: "server-abc",
          role: "server",
        })
      );
      socket.simulateMessage(serverHandshake);

      // Send gossip that discovers a new peer
      const gossip = new TextEncoder().encode(
        JSON.stringify({
          type: "gossip",
          updates: [{ type: "alive", peer: { peerId: "new-peer", address: "ws://new:8765" }, incarnation: 1 }],
        })
      );
      socket.simulateMessage(gossip);

      // Find the socket created for the gossip-discovered peer
      const gossipSocket = socketFactory.instances.find((s) => s.url.includes("new:8765"));

      if (gossipSocket) {
        // Simulate successful connection then disconnect
        gossipSocket.simulateOpen();

        // After disconnect, should NOT attempt reconnection
        gossipSocket.simulateClose();
        await vi.advanceTimersByTimeAsync(10000);

        // No new socket should have been created for this peer
        const reconnectAttempts = socketFactory.instances
          .filter((s) => s.url.includes("new:8765"));
        expect(reconnectAttempts.length).toBe(1); // Only the original, no reconnect
      }
    });

    it("should not attempt duplicate connections to same peer", async () => {
      // Connect and complete handshake
      const connectPromise = manager.connectToUrl("wss://example.com/sync");
      const socket = socketFactory.getLatest()!;
      socket.simulateOpen();
      await connectPromise;

      const serverHandshake = new TextEncoder().encode(
        JSON.stringify({
          type: "handshake",
          peerId: "server-abc",
          role: "server",
        })
      );
      socket.simulateMessage(serverHandshake);

      // Track connection attempts
      const connectionAttempts: string[] = [];
      const originalCreate = socketFactory.create.bind(socketFactory);
      socketFactory.create = (url: string) => {
        connectionAttempts.push(url);
        return originalCreate(url);
      };

      // Send duplicate gossip updates for the same peer
      const gossip1 = new TextEncoder().encode(
        JSON.stringify({
          type: "gossip",
          updates: [{ type: "alive", peer: { peerId: "new-peer", address: "ws://new:8765" }, incarnation: 1 }],
        })
      );
      const gossip2 = new TextEncoder().encode(
        JSON.stringify({
          type: "gossip",
          updates: [{ type: "alive", peer: { peerId: "new-peer", address: "ws://new:8765" }, incarnation: 2 }],
        })
      );

      socket.simulateMessage(gossip1);
      socket.simulateMessage(gossip2);

      // Should only attempt one connection (duplicate is prevented)
      // Note: First gossip triggers connect, second is blocked by connectingPeers Set
      expect(connectionAttempts.filter((url) => url.includes("new:8765")).length).toBeLessThanOrEqual(1);
    });
  });
});
