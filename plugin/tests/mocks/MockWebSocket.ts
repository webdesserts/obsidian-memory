/**
 * Mock WebSocket for testing PeerManager connection lifecycle.
 * Allows programmatic control of connection events (open, close, message, error).
 */
export class MockWebSocket {
  // WebSocket interface
  onopen: (() => void) | null = null;
  onclose: ((event: { code: number; reason: string }) => void) | null = null;
  onmessage: ((event: { data: ArrayBuffer }) => void) | null = null;
  onerror: ((event: Event) => void) | null = null;
  binaryType: string = "arraybuffer";
  readyState: number = MockWebSocket.CONNECTING;

  // Test inspection
  readonly url: string;
  readonly sentMessages: Uint8Array[] = [];

  static readonly CONNECTING = 0;
  static readonly OPEN = 1;
  static readonly CLOSING = 2;
  static readonly CLOSED = 3;

  constructor(url: string) {
    this.url = url;
  }

  send(data: ArrayBuffer | Uint8Array): void {
    if (this.readyState !== MockWebSocket.OPEN) {
      throw new Error("WebSocket is not open");
    }
    const bytes = data instanceof ArrayBuffer ? new Uint8Array(data) : data;
    this.sentMessages.push(bytes);
  }

  close(code?: number, reason?: string): void {
    this.readyState = MockWebSocket.CLOSED;
    this.onclose?.({ code: code ?? 1000, reason: reason ?? "" });
  }

  // Test helpers
  simulateOpen(): void {
    this.readyState = MockWebSocket.OPEN;
    this.onopen?.();
  }

  simulateMessage(data: Uint8Array): void {
    this.onmessage?.({
      data: data.buffer.slice(data.byteOffset, data.byteOffset + data.byteLength),
    });
  }

  simulateClose(code = 1000, reason = ""): void {
    this.readyState = MockWebSocket.CLOSED;
    this.onclose?.({ code, reason });
  }

  simulateError(message = "Connection error"): void {
    this.onerror?.(new Event("error"));
  }

  getLastSentMessage(): Uint8Array | undefined {
    return this.sentMessages[this.sentMessages.length - 1];
  }

  getLastSentJson<T>(): T | undefined {
    const msg = this.getLastSentMessage();
    if (!msg) return undefined;
    return JSON.parse(new TextDecoder().decode(msg));
  }

  clearSentMessages(): void {
    this.sentMessages.length = 0;
  }
}

/**
 * Factory that creates MockWebSocket instances and tracks them.
 * Use this to stub the global WebSocket constructor.
 */
export class MockWebSocketFactory {
  readonly instances: MockWebSocket[] = [];

  create(url: string): MockWebSocket {
    const socket = new MockWebSocket(url);
    this.instances.push(socket);
    return socket;
  }

  getLatest(): MockWebSocket | undefined {
    return this.instances[this.instances.length - 1];
  }

  clear(): void {
    this.instances.length = 0;
  }
}
