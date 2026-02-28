import { describe, it, expect } from "vitest";
import { inferProtocol } from "../src/network/url";

describe("inferProtocol()", () => {
  it("should prefix bare hostname with wss://", () => {
    expect(inferProtocol("umbra.computer")).toBe("wss://umbra.computer");
  });

  it("should prefix bare hostname with path", () => {
    expect(inferProtocol("umbra.computer/sync")).toBe("wss://umbra.computer/sync");
  });

  it("should prefix bare IP with port", () => {
    expect(inferProtocol("192.168.1.100:8765")).toBe("wss://192.168.1.100:8765");
  });

  it("should preserve explicit wss://", () => {
    expect(inferProtocol("wss://example.com")).toBe("wss://example.com");
  });

  it("should preserve explicit ws://", () => {
    expect(inferProtocol("ws://local:8765")).toBe("ws://local:8765");
  });

  it("should handle case-insensitive protocol prefix", () => {
    expect(inferProtocol("WSS://Example.com")).toBe("WSS://Example.com");
    expect(inferProtocol("WS://local")).toBe("WS://local");
  });

  it("should trim whitespace", () => {
    expect(inferProtocol("  umbra.computer  ")).toBe("wss://umbra.computer");
  });

  it("should reject http:// URLs", () => {
    expect(() => inferProtocol("http://umbra.computer/sync")).toThrow("not an HTTP URL");
  });

  it("should reject https:// URLs", () => {
    expect(() => inferProtocol("https://umbra.computer/sync")).toThrow("not an HTTP URL");
  });
});
