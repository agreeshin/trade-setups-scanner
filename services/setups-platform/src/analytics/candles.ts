import { fetchCatalog, type CatalogInstrument } from "../platform-catalog.js";
import type { AnalyticsSetup, OhlcCandle } from "./engine.js";

interface ApiCandlesResponse {
  candles: {
    openTimeUnix: number;
    open: string;
    high: string;
    low: string;
    close: string;
  }[];
  nextCursorUnix: number | null;
}

const PAGE_LIMIT = 5000;
/** Bounded in-memory window cache: max candles retained across instruments. */
const MAX_CACHED_CANDLES = 2_000_000;

interface CacheEntry {
  candles: OhlcCandle[];
  touchedAt: number;
}

const windowCache = new Map<string, CacheEntry>();
let cachedCandleCount = 0;

function rememberWindow(key: string, candles: OhlcCandle[]): void {
  const existing = windowCache.get(key);
  if (existing !== undefined) {
    cachedCandleCount -= existing.candles.length;
    windowCache.delete(key);
  }
  while (cachedCandleCount + candles.length > MAX_CACHED_CANDLES && windowCache.size > 0) {
    const oldestKey = windowCache.keys().next().value;
    if (oldestKey === undefined) break;
    const oldest = windowCache.get(oldestKey);
    if (oldest !== undefined) cachedCandleCount -= oldest.candles.length;
    windowCache.delete(oldestKey);
  }
  windowCache.set(key, { candles, touchedAt: Date.now() });
  cachedCandleCount += candles.length;
}

/** Test-only hook to drop the in-memory window cache. */
export function resetCandleWindowCache(): void {
  windowCache.clear();
  cachedCandleCount = 0;
}

export class MarketDataCandleReader {
  private catalogPromise: Promise<Map<string, number>> | null = null;

  constructor(
    private readonly baseUrl: string,
    private readonly apiKey: string,
  ) {}

  private catalogKey(instrument: { exchangeCode: string; category: string; symbol: string }): string {
    return `${instrument.exchangeCode}\u0000${instrument.category}\u0000${instrument.symbol}`;
  }

  /** Lazily loaded (exchange, category, symbol) -> instrumentId map. */
  private async catalog(): Promise<Map<string, number>> {
    if (this.catalogPromise === null) {
      this.catalogPromise = fetchCatalog(this.baseUrl, this.apiKey).then((instruments: CatalogInstrument[]) => {
        const map = new Map<string, number>();
        for (const instrument of instruments) {
          map.set(`${instrument.exchange}\u0000${instrument.category}\u0000${instrument.symbol}`, instrument.id);
        }
        return map;
      });
      this.catalogPromise.catch(() => {
        this.catalogPromise = null;
      });
    }
    return this.catalogPromise;
  }

  async instrumentId(setup: Pick<AnalyticsSetup, "exchangeCode" | "category" | "symbol">): Promise<number | null> {
    const catalog = await this.catalog();
    return catalog.get(this.catalogKey(setup)) ?? null;
  }

  /**
   * Read all minute candles of one instrument inside [fromUnix, toUnix).
   * Windows are rounded to hour boundaries so repeated matrix cells over the
   * same set reuse the same in-memory window.
   */
  async readWindow(instrumentId: number, fromUnix: number, toUnix: number): Promise<OhlcCandle[]> {
    const alignedFrom = Math.floor(fromUnix / 3600) * 3600;
    const alignedTo = Math.ceil(toUnix / 3600) * 3600;
    const key = `${instrumentId}:${alignedFrom}:${alignedTo}`;
    const cached = windowCache.get(key);
    if (cached !== undefined) {
      cached.touchedAt = Date.now();
      return cached.candles;
    }
    const candles: OhlcCandle[] = [];
    let afterUnix: number | undefined;
    for (;;) {
      const params = new URLSearchParams({
        instrumentId: String(instrumentId),
        fromUnix: String(alignedFrom),
        toUnix: String(alignedTo),
        limit: String(PAGE_LIMIT),
      });
      if (afterUnix !== undefined) params.set("afterUnix", String(afterUnix));
      const response = await fetch(`${this.baseUrl}/v1/candles?${params.toString()}`, {
        headers: { "x-api-key": this.apiKey },
        signal: AbortSignal.timeout(30_000),
      });
      if (!response.ok) {
        throw new Error(`market data api ${response.status}: ${(await response.text()).slice(0, 200)}`);
      }
      const body = (await response.json()) as ApiCandlesResponse;
      for (const candle of body.candles) {
        candles.push({
          openTimeUnix: candle.openTimeUnix,
          open: Number(candle.open),
          high: Number(candle.high),
          low: Number(candle.low),
          close: Number(candle.close),
        });
      }
      if (body.nextCursorUnix === null || body.candles.length === 0) break;
      afterUnix = body.nextCursorUnix;
    }
    candles.sort((a, b) => a.openTimeUnix - b.openTimeUnix);
    rememberWindow(key, candles);
    return candles;
  }
}
