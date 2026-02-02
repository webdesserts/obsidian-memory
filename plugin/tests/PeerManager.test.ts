/**
 * PeerManager reconnection tests.
 *
 * These tests verify that PeerManager correctly handles WebSocket
 * reconnection - specifically that handshakes are sent after reconnect.
 */

import { describe, it, expect, beforeEach, vi, afterEach } from "vitest";
import { MockWebSocket, MockWebSocketFactory } from "./mocks/MockWebSocket";
import { PeerManager } from "../src/network/PeerManager";
import { configureLogger } from "../src/logger";

// Suppress log output during tests
configureLogger({ level: "none" });

describe("PeerManager", () => {
  let manager: PeerManager;
  let socketFactory: MockWebSocketFactory;

  beforeEach(() => {
    vi.useFakeTimers();

    socketFactory = new MockWebSocketFactory();

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
  });

  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  describe("connectToUrl()", () => {
    describe("Given a fresh PeerManager", () => {
      describe("When connecting to a URL", () => {
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

        it("should emit peer-connected event", async () => {
          const events: { direction: string }[] = [];
          manager.on("peer-connected", (info) => events.push(info));

          const connectPromise = manager.connectToUrl("wss://example.com/sync");
          socketFactory.getLatest()!.simulateOpen();
          await connectPromise;

          expect(events).toHaveLength(1);
          expect(events[0].direction).toBe("outgoing");
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

        it("should emit peer-connected event on reconnect", async () => {
          const events: { id: string; direction: string }[] = [];
          manager.on("peer-connected", (info) => events.push(info));

          // Already connected once in beforeEach, so clear
          events.length = 0;

          // Disconnect and reconnect
          firstSocket.simulateClose();
          await vi.advanceTimersByTimeAsync(5000);
          socketFactory.getLatest()!.simulateOpen();

          // Should have emitted peer-connected on reconnect
          expect(events).toHaveLength(1);
          expect(events[0].direction).toBe("outgoing");
        });

        it("should clear stale ID mappings from previous connection", async () => {
          // Simulate receiving server handshake (creates ID mappings)
          const serverHandshake = new TextEncoder().encode(
            JSON.stringify({
              type: "handshake",
              peerId: "server-xyz",
              role: "server",
            })
          );
          firstSocket.simulateMessage(serverHandshake);

          // Verify the mapping was created (peer shows as server-xyz)
          let peers = manager.getConnectedPeers();
          expect(peers.some((p) => p.id === "server-xyz")).toBe(true);

          // Disconnect and reconnect
          firstSocket.simulateClose();
          await vi.advanceTimersByTimeAsync(5000);
          socketFactory.getLatest()!.simulateOpen();

          // After reconnect, old mapping should be cleared - peer uses temp ID again
          peers = manager.getConnectedPeers();
          expect(peers.some((p) => p.id === "server-xyz")).toBe(false);
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
        const originalSend = socket.send.bind(socket);
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
});
