/**
 * QUIC variable-length integer encoding and decoding (RFC 9000, Section 16).
 *
 * The 2 most significant bits of the first byte encode the number of additional
 * bytes that make up the integer:
 *
 *  00 → 1-byte, max value 63
 *  01 → 2-byte, max value 16383
 *  10 → 4-byte, max value 1073741823
 *  11 → 8-byte, max value 4611686018427387903
 *
 * All multi-byte values are big-endian with the 2-bit prefix stripped.
 */

/** Maximum value representable in each encoding width. */
const MAX_1 = 63;
const MAX_2 = 16383;
const MAX_4 = 1073741823;

/**
 * Encodes a non-negative integer using the QUIC variable-length encoding.
 * Supports values up to MAX_SAFE_INTEGER (2^53 - 1).
 */
export function encodeVarInt(value: number): Uint8Array {
  if (value < 0) throw new RangeError(`VarInt value must be non-negative, got ${value}`);

  if (value <= MAX_1) {
    return new Uint8Array([value]);
  }

  if (value <= MAX_2) {
    const buf = new Uint8Array(2);
    const view = new DataView(buf.buffer);
    view.setUint16(0, value | 0x4000, false);
    return buf;
  }

  if (value <= MAX_4) {
    const buf = new Uint8Array(4);
    const view = new DataView(buf.buffer);
    view.setUint32(0, value | 0x80000000, false);
    return buf;
  }

  // 8-byte encoding for large values. JavaScript numbers are IEEE 754 doubles,
  // so we can represent integers exactly up to 2^53. We split across two u32s.
  const buf = new Uint8Array(8);
  const view = new DataView(buf.buffer);
  // High 32 bits: prefix 11 (top 2 bits) + bits 32-53 of value
  const hi = Math.floor(value / 0x100000000);
  const lo = value >>> 0;
  view.setUint32(0, hi | 0xc0000000, false);
  view.setUint32(4, lo, false);
  return buf;
}

/** Result of decoding a single VarInt from a buffer. */
export interface VarIntDecodeResult {
  value: number;
  bytesRead: number;
}

/**
 * Decodes a QUIC variable-length integer from `buf` starting at `offset`.
 * Throws if the buffer is too short for the encoded width.
 */
export function decodeVarInt(buf: Uint8Array, offset: number): VarIntDecodeResult {
  if (offset >= buf.length) {
    throw new RangeError(`VarInt decode: offset ${offset} is out of bounds (length ${buf.length})`);
  }

  const view = new DataView(buf.buffer, buf.byteOffset, buf.byteLength);
  const prefix = (buf[offset] & 0xc0) >> 6;

  switch (prefix) {
    case 0: {
      return { value: buf[offset], bytesRead: 1 };
    }
    case 1: {
      if (offset + 2 > buf.length) throw new RangeError("VarInt decode: truncated 2-byte integer");
      const raw = view.getUint16(offset, false);
      return { value: raw & 0x3fff, bytesRead: 2 };
    }
    case 2: {
      if (offset + 4 > buf.length) throw new RangeError("VarInt decode: truncated 4-byte integer");
      const raw = view.getUint32(offset, false);
      return { value: raw & 0x3fffffff, bytesRead: 4 };
    }
    case 3: {
      if (offset + 8 > buf.length) throw new RangeError("VarInt decode: truncated 8-byte integer");
      const hi = view.getUint32(offset, false) & 0x3fffffff;
      const lo = view.getUint32(offset + 4, false);
      return { value: hi * 0x100000000 + lo, bytesRead: 8 };
    }
    default:
      throw new RangeError(`VarInt decode: unexpected prefix ${prefix}`);
  }
}
