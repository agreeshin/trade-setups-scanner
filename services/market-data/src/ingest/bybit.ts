import type { AdaptiveGate } from "./gate.js";
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

interface BybitKlineRow {
  list?: [string, string, string, string, string, string, string][];
}

const MAX_LIMIT = 1000;
const MINUTE_MS = 60_000;
const MAX_ATTEMPTS_PER_PAGE = 6;

function parseRetryAfterMs(headers: Map<string, string>): number | undefined {
  const value = headers.get("retry-after");
  if (value === undefined) {
    return undefined;
  }
  const seconds = Number(value);
  return Number.isFinite(seconds) && seconds > 0 ? seconds * 1000 : undefined;
}

function isAccessTooFrequent(status: number, bodyText: string): boolean {
  return status === 403 && bodyText.toLowerCase().includes("access too frequent");
}

export class BybitClient {
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
  ): Promise<KlineBar[]> {
    const collected = new Map<number, KlineBar>();
    const windowMinutes = MAX_LIMIT - 1;
    let cursor = fromMs;

    while (cursor <= toMs) {
      const windowStart = cursor;
      const windowEnd = Math.min(toMs, windowStart + windowMinutes * MINUTE_MS);
      const rows = await this.fetchWindowWithRetry(symbol, windowStart, windowEnd);
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
  ): Promise<RawPageBar[]> {
    let lastError: unknown = undefined;
    for (let attempt = 0; attempt < MAX_ATTEMPTS_PER_PAGE; attempt += 1) {
      await this.gate.acquire();
      try {
        const url =
          `${this.baseUrl}/v5/market/kline?category=linear&symbol=${encodeURIComponent(symbol)}` +
          `&interval=1&start=${startMs}&end=${endMs}&limit=${MAX_LIMIT}`;
        const response = await this.transport.fetch(url);

        if (response.status === 429) {
          this.gate.reportThrottle(parseRetryAfterMs(response.headers));
          lastError = new BybitApiError("http-429", "HTTP 429 frequency protection");
          continue;
        }

        if (response.status === 403) {
          if (isAccessTooFrequent(response.status, response.bodyText)) {
            const serverWait = parseRetryAfterMs(response.headers);
            await this.gate.reportIpBan(serverWait);
            lastError = new BybitApiError("ip-ban", "403 access too frequent");
            continue;
          }
          throw new BybitApiError(
            "forbidden",
            `403 without access-too-frequent marker: ${response.bodyText.slice(0, 200)}`,
          );
        }

        if (response.status !== 200) {
          throw new BybitApiError(
            "api-error",
            `unexpected HTTP ${response.status}: ${response.bodyText.slice(0, 200)}`,
          );
        }

        const payload = JSON.parse(response.bodyText) as {
          retCode: number;
          retMsg: string;
          result?: BybitKlineRow;
        };

        if (payload.retCode === 10006) {
          this.gate.reportThrottle();
          lastError = new BybitApiError("rate-limit", "retCode 10006 API rate limit");
          continue;
        }

        if (payload.retCode !== 0) {
          throw new BybitApiError(
            "api-error",
            `retCode ${payload.retCode}: ${payload.retMsg}`,
          );
        }

        this.gate.reportSuccess();
        const list = payload.result?.list ?? [];
        return list.map((row) => ({
          openTimeMs: Number(row[0]),
          open: row[1],
          high: row[2],
          low: row[3],
          close: row[4],
          volume: row[5],
          quoteTurnover: row[6],
        }));
      } catch (error) {
        if (error instanceof BybitApiError && error.kind !== "network") {
          throw error;
        }
        lastError = error;
        continue;
      }
    }
    throw lastError instanceof Error
      ? lastError
      : new BybitApiError("network", "page fetch failed after retries");
  }
}
