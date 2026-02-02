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
});
