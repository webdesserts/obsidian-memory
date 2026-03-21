import { describe, it, expect } from "vitest";
import { encodeVarInt, decodeVarInt } from "../src/varint.js";

describe("encodeVarInt / decodeVarInt", () => {
  describe("round-trip encoding", () => {
    const cases: [string, number][] = [
      ["0 (1-byte min)", 0],
      ["1", 1],
      ["63 (1-byte max)", 63],
      ["64 (2-byte min)", 64],
      ["16383 (2-byte max)", 16383],
      ["16384 (4-byte min)", 16384],
      ["1073741823 (4-byte max)", 1073741823],
    ];

    for (const [label, value] of cases) {
      it(`round-trips ${label}`, () => {
        const encoded = encodeVarInt(value);
        const { value: decoded, bytesRead } = decodeVarInt(encoded, 0);
        expect(decoded).toBe(value);
        expect(bytesRead).toBe(encoded.length);
      });
    }
  });

  describe("encoding widths", () => {
    it("encodes values 0–63 in 1 byte", () => {
      expect(encodeVarInt(0)).toHaveLength(1);
      expect(encodeVarInt(63)).toHaveLength(1);
    });

    it("encodes values 64–16383 in 2 bytes", () => {
      expect(encodeVarInt(64)).toHaveLength(2);
      expect(encodeVarInt(16383)).toHaveLength(2);
    });

    it("encodes values 16384–1073741823 in 4 bytes", () => {
      expect(encodeVarInt(16384)).toHaveLength(4);
      expect(encodeVarInt(1073741823)).toHaveLength(4);
    });

    it("encodes values > 1073741823 in 8 bytes", () => {
      expect(encodeVarInt(1073741824)).toHaveLength(8);
    });
  });

  describe("prefix bits", () => {
    it("1-byte prefix is 0b00", () => {
      expect(encodeVarInt(42)[0] & 0xc0).toBe(0x00);
    });

    it("2-byte prefix is 0b01", () => {
      expect(encodeVarInt(100)[0] & 0xc0).toBe(0x40);
    });

    it("4-byte prefix is 0b10", () => {
      expect(encodeVarInt(20000)[0] & 0xc0).toBe(0x80);
    });

    it("8-byte prefix is 0b11", () => {
      expect(encodeVarInt(2_000_000_000)[0] & 0xc0).toBe(0xc0);
    });
  });

  describe("offset support", () => {
    it("decodes starting at a non-zero offset", () => {
      const padding = new Uint8Array([0xff, 0xff]);
      const value = encodeVarInt(42);
      const buf = new Uint8Array([...padding, ...value]);
      const result = decodeVarInt(buf, 2);
      expect(result.value).toBe(42);
      expect(result.bytesRead).toBe(1);
    });
  });

  describe("error handling", () => {
    it("throws on offset out of bounds", () => {
      const buf = new Uint8Array([0x01]);
      expect(() => decodeVarInt(buf, 1)).toThrow(RangeError);
    });

    it("throws on truncated 2-byte integer", () => {
      // First byte with 01 prefix but only one byte available
      const buf = new Uint8Array([0x40]);
      expect(() => decodeVarInt(buf, 0)).toThrow(RangeError);
    });

    it("throws on truncated 4-byte integer", () => {
      const buf = new Uint8Array([0x80, 0x00, 0x00]);
      expect(() => decodeVarInt(buf, 0)).toThrow(RangeError);
    });

    it("throws on truncated 8-byte integer", () => {
      const buf = new Uint8Array([0xc0, 0x00, 0x00, 0x00]);
      expect(() => decodeVarInt(buf, 0)).toThrow(RangeError);
    });

    it("throws on negative input", () => {
      expect(() => encodeVarInt(-1)).toThrow(RangeError);
    });
  });
});
