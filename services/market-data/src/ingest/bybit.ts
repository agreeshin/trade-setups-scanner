import type { AdaptiveGate, RequestPriority } from "./gate.js";
import type { Clock } from "./clock.js";
import type { Transport } from "./transport.js";

export type BybitErrorKind =
  | "rate-limit"
  | "http-429"
  | "ip-ban"
  | "forbidden"
  | "api-error"
  | "network";

export interface KlineBar {
  openTime: Date;
  open: string;
  high: string;
  low: string;
  close: string;
  volume: string;
  quoteTurnover: string;
}

type RawPageBar = { openTimeMs: number } & Omit<KlineBar, "openTime">;

export class BybitApiError extends Error {
  constructor(
    public readonly kind: BybitErrorKind,
    message: string,
  ) {
    super(message);
    this.name = "BybitApiError";
  }
}

const MAX_LIMIT = 1000;
const MINUTE_MS = 60_000;
const MAX_ATTEMPTS_PER_PAGE = 6;

function parseRetryAfterMs(headers: Map<string, string>, nowMs: number): number | undefined {
  const value = headers.get("retry-after");
  if (value === undefined) {
    return undefined;
  }
  const seconds = Number(value);
  if (Number.isFinite(seconds) && seconds >= 0) return seconds * 1000;
  const date = Date.parse(value);
  return Number.isFinite(date) ? Math.max(0, date - nowMs) : undefined;
}

export interface BybitRequestOptions {
  priority?: RequestPriority;
  signal?: AbortSignal | undefined;
  maxAttempts?: number;
}

export function validateBar(bar: KlineBar): void {
  const time = bar.openTime.getTime();
  if (!Number.isSafeInteger(time) || time % MINUTE_MS !== 0) throw new Error("invalid kline minute timestamp");
  for (const value of [bar.open, bar.high, bar.low, bar.close, bar.volume, bar.quoteTurnover]) {
    if (typeof value !== "string" || value.length > 128 || !/^\d+(?:\.\d+)?$/.test(value)) throw new Error("invalid kline decimal");
  }
}

function isAccessTooFrequent(status: number, bodyText: string): boolean {
  return status === 403 && bodyText.toLowerCase().includes("access too frequent");
}

export class BybitClient {
  private readonly lifetime = new AbortController();
  constructor(
    private readonly transport: Transport,
    private readonly gate: AdaptiveGate,
    private readonly clock: Clock,
    private readonly baseUrl = "https://api.bybit.com",
  ) {}

  async fetchClosedBars(
    symbol: string,
    fromMs: number,
    toMs: number,
    options: BybitRequestOptions = {},
  ): Promise<KlineBar[]> {
    const collected = new Map<number, RawPageBar>();
    const windowMinutes = MAX_LIMIT - 1;
    let cursor = fromMs;

    while (cursor <= toMs) {
      const windowStart = cursor;
      const windowEnd = Math.min(toMs, windowStart + windowMinutes * MINUTE_MS);
      const rows = await this.fetchWindowWithRetry(symbol, windowStart, windowEnd, options);
      const lastClosedMinuteStart =
        Math.floor(this.clock.nowMs() / MINUTE_MS) * MINUTE_MS;
      for (const bar of rows) {
        if (bar.openTimeMs >= lastClosedMinuteStart) {
          continue;
        }
        if (bar.openTimeMs >= windowStart && bar.openTimeMs <= windowEnd) {
          collected.set(bar.openTimeMs, bar);
        }
      }
      cursor = windowEnd + MINUTE_MS;
    }

    return [...collected.entries()]
      .sort(([a], [b]) => a - b)
      .map(([openTimeMs, bar]) => ({
        ...bar,
        openTime: new Date(openTimeMs),
      }));
  }

  private async fetchWindowWithRetry(
    symbol: string,
    startMs: number,
    endMs: number,
    options: BybitRequestOptions,
  ): Promise<RawPageBar[]> {
    const url =
      `${this.baseUrl}/v5/market/kline?category=linear&symbol=${encodeURIComponent(symbol)}` +
      `&interval=1&start=${startMs}&end=${endMs}&limit=${MAX_LIMIT}`;
    const { list } = await this.fetchResult<[string, string, string, string, string, string, string]>(url, options);
    if (list.length > MAX_LIMIT) throw new Error("Bybit kline page exceeds item limit");
    return list.map((row) => {
      if (!Array.isArray(row) || row.length !== 7) throw new Error("invalid Bybit kline row");
      const bar = {
      openTimeMs: Number(row[0]),
      open: row[1],
      high: row[2],
      low: row[3],
      close: row[4],
      volume: row[5],
      quoteTurnover: row[6],
      };
      validateBar({ ...bar, openTime: new Date(bar.openTimeMs) });
      return bar;
    });
  }

  async fetchLinearPerpetualUsdtSymbols(): Promise<string[]> {
    const symbols = new Set<string>();
    let cursor: string | null = null;
    for (let page = 0; page < 10; page += 1) {
      const url: string =
        `${this.baseUrl}/v5/market/instruments-info?category=linear&limit=1000` +
        (cursor === null ? "" : `&cursor=${encodeURIComponent(cursor)}`);
      const { list, nextPageCursor } = await this.fetchResult<{
        symbol: string;
        contractType?: string;
        quoteCoin?: string;
        status: string;
      }>(url);
      for (const row of list) {
        if (row.status === "Trading" && row.contractType === "LinearPerpetual" && row.quoteCoin === "USDT") {
          symbols.add(row.symbol);
        }
      }
      if (!nextPageCursor) break;
      cursor = nextPageCursor;
    }
    return [...symbols].sort();
  }

  close(): void {
    this.lifetime.abort(new Error("Bybit client stopped"));
    this.gate.stop();
    this.transport.abortActive?.();
  }

  get closedMinuteStartMs(): number { return Math.floor(this.clock.nowMs() / MINUTE_MS) * MINUTE_MS - MINUTE_MS; }

  private async fetchResult<TRow>(url: string, options: BybitRequestOptions = {}): Promise<{ list: TRow[]; nextPageCursor: string | undefined }> {
    let lastError: unknown = undefined;
    const signal = options.signal ? AbortSignal.any([options.signal, this.lifetime.signal]) : this.lifetime.signal;
    for (let attempt = 0; attempt < (options.maxAttempts ?? MAX_ATTEMPTS_PER_PAGE); attempt += 1) {
      signal.throwIfAborted();
      try {
        return await this.gate.run(async () => {
        const response = await this.transport.fetch(url, signal);
        const retryAfter = parseRetryAfterMs(response.headers, this.clock.nowMs());

        if (response.status === 429) {
          await this.gate.reportThrottle(Math.max(1000, retryAfter ?? 0));
          throw new BybitApiError("http-429", "HTTP 429 frequency protection");
        }

        if (response.status === 403) {
          if (isAccessTooFrequent(response.status, response.bodyText)) {
            this.transport.abortActive?.();
            await this.gate.reportIpBan(retryAfter);
            throw new BybitApiError("ip-ban", "403 access too frequent");
          }
          throw new BybitApiError(
            "forbidden",
            `403 without access-too-frequent marker: ${response.bodyText.slice(0, 200)}`,
          );
        }

        if (response.status !== 200) {
          throw new BybitApiError(
            response.status >= 500 ? "network" : "api-error",
            `unexpected HTTP ${response.status}: ${response.bodyText.slice(0, 200)}`,
          );
        }

        const payload = JSON.parse(response.bodyText) as {
          retCode: number;
          retMsg: string;
          result?: { list?: TRow[]; nextPageCursor?: string };
        };

        if (payload.retCode === 10006) {
          const reset = Number(response.headers.get("x-bapi-limit-reset-timestamp"));
          await this.gate.reportThrottle(Math.max(1000, retryAfter ?? 0, Number.isFinite(reset) ? reset - this.clock.nowMs() : 0));
          throw new BybitApiError("rate-limit", "retCode 10006 API rate limit");
        }

        if (payload.retCode !== 0) {
          throw new BybitApiError(
            "api-error",
            `retCode ${payload.retCode}: ${payload.retMsg}`,
          );
        }

        if (!Array.isArray(payload.result?.list)) throw new BybitApiError("api-error", "invalid Bybit result list");
        this.gate.reportSuccess();
        return { list: payload.result.list, nextPageCursor: payload.result.nextPageCursor };
        }, options.priority ?? "history", signal);
      } catch (error) {
        signal.throwIfAborted();
        if (error instanceof BybitApiError && (error.kind === "forbidden" || error.kind === "api-error")) {
          throw error;
        }
        lastError = error;
        if (!(error instanceof BybitApiError) || error.kind === "network") await this.clock.sleep(Math.min(2000, 100 * 2 ** attempt), signal);
      }
    }
    throw lastError instanceof Error
      ? lastError
      : new BybitApiError("network", "page fetch failed after retries");
  }
}
