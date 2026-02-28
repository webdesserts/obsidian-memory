/**
 * Infer WebSocket protocol for a peer address.
 *
 * WebSocket has no automatic protocol upgrade like HTTP→HTTPS, so bare
 * addresses default to `wss://`. Users who specifically need unencrypted
 * connections can type `ws://` explicitly.
 *
 * Throws if the input uses `http://` or `https://` — these are not
 * WebSocket protocols and would produce a broken URL if prefixed.
 */
export function inferProtocol(input: string): string {
  const trimmed = input.trim();
  const lower = trimmed.toLowerCase();

  if (lower.startsWith("http://") || lower.startsWith("https://")) {
    throw new Error(
      "Use a WebSocket address (wss://), not an HTTP URL. " +
      "Try removing the http:// or https:// prefix."
    );
  }

  // Already has a protocol — use as-is
  if (lower.startsWith("ws://") || lower.startsWith("wss://")) {
    return trimmed;
  }

  return `wss://${trimmed}`;
}
