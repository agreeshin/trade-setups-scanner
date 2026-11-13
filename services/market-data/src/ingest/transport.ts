export interface TransportResponse {
  status: number;
  headers: Map<string, string>;
  bodyText: string;
}
export interface Transport {
  fetch(url: string, signal?: AbortSignal): Promise<TransportResponse>;
  abortActive?(): void;
}

/** Response timeout includes the entire body, not just headers. */
export class FetchTransport implements Transport {
  private readonly active = new Set<AbortController>();
  constructor(private readonly timeoutMs = 5000, private readonly maxBodyBytes = 2 * 1024 * 1024) {}

  abortActive(): void {
    for (const controller of this.active) controller.abort(new Error("Bybit HTTP requests aborted"));
  }

  async fetch(url: string, signal?: AbortSignal): Promise<TransportResponse> {
    const controller = new AbortController();
    this.active.add(controller);
    const timeout = setTimeout(() => controller.abort(new Error("Bybit request timeout")), this.timeoutMs);
    const combined = signal ? AbortSignal.any([signal, controller.signal]) : controller.signal;
    try {
      combined.throwIfAborted();
      const response = await fetch(url, { signal: combined });
      const headers = new Map<string, string>();
      for (const [key, value] of response.headers.entries()) headers.set(key.toLowerCase(), value);
      const reader = response.body?.getReader();
      const chunks: Uint8Array[] = [];
      let bytes = 0;
      try {
        if (reader) for (;;) {
          const chunk: { done?: boolean; value?: unknown } = await reader.read();
          if (chunk.done) break;
          const value: unknown = chunk.value;
          if (!(value instanceof Uint8Array)) throw new Error("invalid Bybit response chunk");
          bytes += value.byteLength;
          if (bytes > this.maxBodyBytes) throw new Error("Bybit response exceeds byte limit");
          chunks.push(value);
        }
      } finally {
        await reader?.cancel().catch(() => undefined);
        reader?.releaseLock();
      }
      return { status: response.status, headers, bodyText: Buffer.concat(chunks, bytes).toString("utf8") };
    } finally {
      clearTimeout(timeout);
      this.active.delete(controller);
    }
  }
}
