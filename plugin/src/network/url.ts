/**
 * Infer WebSocket protocol for a peer address.
 *
 * WebSocket has no automatic protocol upgrade like HTTP→HTTPS, so bare
 * addresses default to `wss://`. Users who specifically need unencrypted
 * connections can type `ws://` explicitly.
 */
export function inferProtocol(input: string): string {
  const trimmed = input.trim();
  const lower = trimmed.toLowerCase();

  // Already has a protocol — use as-is
  if (lower.startsWith("ws://") || lower.startsWith("wss://")) {
    return trimmed;
  }

  return `wss://${trimmed}`;
}
