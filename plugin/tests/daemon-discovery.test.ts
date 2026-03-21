/**
 * DaemonDiscovery tests.
 *
 * Validates that DaemonDiscovery correctly reads relay URLs from daemon.toml
 * and emits relay-changed events when the URL changes.
 */

import { describe, it, expect, beforeEach, vi } from "vitest";
import { DaemonDiscovery } from "../src/network/daemon-discovery";

// Mock the obsidian Events base class (already aliased in vitest config)
// DaemonDiscovery extends Events from obsidian, which is mocked in tests/mocks/obsidian.ts

function makeMockAdapter(overrides: Partial<{
  exists: (path: string) => Promise<boolean>;
  read: (path: string) => Promise<string>;
}> = {}) {
  return {
    exists: vi.fn<[string], Promise<boolean>>().mockResolvedValue(false),
    read: vi.fn<[string], Promise<string>>().mockResolvedValue(""),
    ...overrides,
  };
}

describe("DaemonDiscovery", () => {
  let discovery: DaemonDiscovery;

  beforeEach(() => {
    vi.useFakeTimers();
    discovery = new DaemonDiscovery();
  });

  afterEach(() => {
    discovery.stop();
    vi.useRealTimers();
  });

  describe("start()", () => {
    it("reads daemon.toml with relay_url and returns the URL", async () => {
      const adapter = makeMockAdapter({
        exists: vi.fn().mockResolvedValue(true),
        read: vi.fn().mockResolvedValue('relay_url = "ws://127.0.0.1:8080/relay"'),
      });

      await discovery.start(adapter);

      expect(discovery.currentRelayUrl).toBe("ws://127.0.0.1:8080/relay");
    });

    it("returns null when daemon.toml exists but has no relay_url", async () => {
      const adapter = makeMockAdapter({
        exists: vi.fn().mockResolvedValue(true),
        read: vi.fn().mockResolvedValue("[daemon]\nsome_other_key = true"),
      });

      await discovery.start(adapter);

      expect(discovery.currentRelayUrl).toBeNull();
    });

    it("returns null when daemon.toml does not exist", async () => {
      const adapter = makeMockAdapter({
        exists: vi.fn().mockResolvedValue(false),
      });

      await discovery.start(adapter);

      expect(discovery.currentRelayUrl).toBeNull();
    });

    it("returns null when daemon.toml is malformed TOML", async () => {
      const adapter = makeMockAdapter({
        exists: vi.fn().mockResolvedValue(true),
        read: vi.fn().mockResolvedValue("this is not = valid = toml ]["),
      });

      await discovery.start(adapter);

      expect(discovery.currentRelayUrl).toBeNull();
    });
  });

  describe("relay-changed event", () => {
    it("emits relay-changed when the relay URL changes between polls", async () => {
      // Start with relay URL present
      const adapter = makeMockAdapter({
        exists: vi.fn().mockResolvedValue(true),
        read: vi.fn()
          .mockResolvedValueOnce('relay_url = "ws://127.0.0.1:8080/relay"')
          .mockResolvedValueOnce('relay_url = "ws://127.0.0.1:9090/relay"'),
      });

      const listener = vi.fn();
      discovery.on("relay-changed", listener);

      await discovery.start(adapter);
      // First call fires on start with the initial URL
      expect(listener).toHaveBeenCalledTimes(1);
      expect(listener).toHaveBeenCalledWith("ws://127.0.0.1:8080/relay");

      // Advance timer to trigger next poll
      await vi.advanceTimersByTimeAsync(5000);

      expect(listener).toHaveBeenCalledTimes(2);
      expect(listener).toHaveBeenLastCalledWith("ws://127.0.0.1:9090/relay");
    });

    it("emits relay-changed with null when relay URL is removed", async () => {
      const adapter = makeMockAdapter({
        exists: vi.fn().mockResolvedValue(true),
        read: vi.fn()
          .mockResolvedValueOnce('relay_url = "ws://127.0.0.1:8080/relay"')
          .mockResolvedValueOnce("[daemon]\n# no relay_url"),
      });

      const listener = vi.fn();
      discovery.on("relay-changed", listener);

      await discovery.start(adapter);
      await vi.advanceTimersByTimeAsync(5000);

      expect(listener).toHaveBeenCalledTimes(2);
      expect(listener).toHaveBeenLastCalledWith(null);
    });

    it("does not emit relay-changed when the URL stays the same between polls", async () => {
      const adapter = makeMockAdapter({
        exists: vi.fn().mockResolvedValue(true),
        read: vi.fn().mockResolvedValue('relay_url = "ws://127.0.0.1:8080/relay"'),
      });

      const listener = vi.fn();
      discovery.on("relay-changed", listener);

      await discovery.start(adapter);
      // Only fires once for the initial read
      expect(listener).toHaveBeenCalledTimes(1);

      await vi.advanceTimersByTimeAsync(5000);

      // Still only once — URL didn't change
      expect(listener).toHaveBeenCalledTimes(1);
    });
  });

  describe("stop()", () => {
    it("stops polling after stop() is called", async () => {
      const read = vi.fn().mockResolvedValue('relay_url = "ws://127.0.0.1:8080/relay"');
      const adapter = makeMockAdapter({
        exists: vi.fn().mockResolvedValue(true),
        read,
      });

      await discovery.start(adapter);
      const callsAfterStart = read.mock.calls.length;

      discovery.stop();
      await vi.advanceTimersByTimeAsync(15000);

      // No additional reads after stop
      expect(read.mock.calls.length).toBe(callsAfterStart);
    });
  });
});
