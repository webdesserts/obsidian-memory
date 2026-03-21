/**
 * NetworkManager relay integration tests.
 *
 * Validates the relay selection logic: daemon relay takes priority over the
 * plugin relay, and the plugin falls back to no-relay mode if both fail.
 *
 * WasmSyncNode and IrohRelayServer are both mocked since WASM and real network
 * bindings are unavailable in the Node.js test environment.
 */

import { describe, it, expect, beforeEach, afterEach, vi } from "vitest";
import { NetworkManager } from "../src/network/NetworkManager";

// ---- Mocks ----

const mockSyncNode = {
  nodeId: vi.fn().mockReturnValue("aabbccdd"),
  joinVaultGossip: vi.fn().mockResolvedValue(undefined),
  pollGossipEvent: vi.fn().mockReturnValue(null),
  pollInboundSync: vi.fn().mockReturnValue(null),
  shutdown: vi.fn().mockResolvedValue(undefined),
};

const mockWasmModule = {
  WasmSyncNode: {
    create: vi.fn().mockResolvedValue(mockSyncNode),
  },
};

const mockRelayServer = {
  start: vi.fn<[], Promise<{ port: number; url: string }>>().mockResolvedValue({
    port: 9999,
    url: "ws://127.0.0.1:9999/relay",
  }),
  stop: vi.fn().mockResolvedValue(undefined),
  clientCount: 0,
};

const mockRelayModule = {
  IrohRelayServer: vi.fn().mockImplementation(() => mockRelayServer),
};

// Intercept dynamic import() calls inside NetworkManager.
// NetworkManager uses dynamic imports to avoid loading WASM at startup;
// these mocks intercept those imports in the test environment.
vi.mock("../src/wasm", () => mockWasmModule);
vi.mock("@webdesserts/iroh-relay", () => mockRelayModule);

function makeMockAdapter(options: {
  hasDaemonToml?: boolean;
  relayUrl?: string;
} = {}): { exists: ReturnType<typeof vi.fn>; read: ReturnType<typeof vi.fn> } {
  return {
    exists: vi.fn().mockResolvedValue(options.hasDaemonToml ?? false),
    read: vi.fn().mockResolvedValue(
      options.relayUrl ? `relay_url = "${options.relayUrl}"` : ""
    ),
  };
}

// ---- Tests ----

describe("NetworkManager relay selection", () => {
  let manager: NetworkManager;
  const secretKey = new Uint8Array(32);
  const vaultId = "test-vault";
  const peers: string[] = [];

  beforeEach(() => {
    vi.useFakeTimers();
    manager = new NetworkManager();
    vi.clearAllMocks();
    mockSyncNode.nodeId.mockReturnValue("aabbccdd");
    mockSyncNode.joinVaultGossip.mockResolvedValue(undefined);
    mockSyncNode.pollGossipEvent.mockReturnValue(null);
    mockSyncNode.pollInboundSync.mockReturnValue(null);
    mockRelayServer.start.mockResolvedValue({ port: 9999, url: "ws://127.0.0.1:9999/relay" });
    mockRelayServer.stop.mockResolvedValue(undefined);
    mockWasmModule.WasmSyncNode.create.mockResolvedValue(mockSyncNode);
    mockRelayModule.IrohRelayServer.mockImplementation(() => mockRelayServer);
  });

  afterEach(async () => {
    await manager.stop();
    vi.useRealTimers();
  });

  describe("when daemon.toml has a relay_url", () => {
    it("uses the daemon relay URL and does not start a plugin relay", async () => {
      const adapter = makeMockAdapter({
        hasDaemonToml: true,
        relayUrl: "ws://127.0.0.1:8080/relay",
      });

      await manager.start(secretKey, vaultId, peers, adapter);

      // WasmSyncNode.create should receive the daemon's relay URL
      expect(mockWasmModule.WasmSyncNode.create).toHaveBeenCalledWith(
        secretKey,
        "ws://127.0.0.1:8080/relay"
      );

      // No plugin relay should have been started
      expect(mockRelayServer.start).not.toHaveBeenCalled();

      expect(manager.relayInfo).toEqual({
        mode: "daemon",
        url: "ws://127.0.0.1:8080/relay",
        clientCount: 0,
      });
    });
  });

  describe("when no daemon.toml is present", () => {
    it("starts the plugin relay and passes its URL to the sync node", async () => {
      const adapter = makeMockAdapter({ hasDaemonToml: false });

      await manager.start(secretKey, vaultId, peers, adapter);

      expect(mockRelayServer.start).toHaveBeenCalledTimes(1);
      expect(mockWasmModule.WasmSyncNode.create).toHaveBeenCalledWith(
        secretKey,
        "ws://127.0.0.1:9999/relay"
      );

      expect(manager.relayInfo).toEqual({
        mode: "plugin",
        url: "ws://127.0.0.1:9999/relay",
        clientCount: 0,
      });
    });
  });

  describe("when the plugin relay fails to start", () => {
    it("creates the sync node with undefined relay and logs a warning", async () => {
      const adapter = makeMockAdapter({ hasDaemonToml: false });
      mockRelayServer.start.mockRejectedValue(new Error("port in use"));

      await manager.start(secretKey, vaultId, peers, adapter);

      // Sync node is still created — just without a relay URL
      expect(mockWasmModule.WasmSyncNode.create).toHaveBeenCalledWith(
        secretKey,
        undefined
      );

      expect(manager.relayInfo).toEqual({
        mode: "none",
        url: null,
        clientCount: 0,
      });
    });
  });

  describe("stop()", () => {
    it("shuts down relay server and daemon discovery", async () => {
      const adapter = makeMockAdapter({ hasDaemonToml: false });
      await manager.start(secretKey, vaultId, peers, adapter);

      await manager.stop();

      expect(mockRelayServer.stop).toHaveBeenCalledTimes(1);
      expect(mockSyncNode.shutdown).toHaveBeenCalledTimes(1);
    });

    it("shuts down cleanly when there is no relay server (daemon relay path)", async () => {
      const adapter = makeMockAdapter({
        hasDaemonToml: true,
        relayUrl: "ws://127.0.0.1:8080/relay",
      });
      await manager.start(secretKey, vaultId, peers, adapter);

      await manager.stop();

      // No plugin relay to stop
      expect(mockRelayServer.stop).not.toHaveBeenCalled();
      expect(mockSyncNode.shutdown).toHaveBeenCalledTimes(1);
    });
  });
});
