export interface CandleBarDto {
  openTimeUnix: number;
  close: string;
  volume: string;
  quoteTurnover: string;
}

export interface CandlePage {
  bars: CandleBarDto[];
  nextCursorUnix: number | null;
}

export interface CandleSourceQuery {
  fromUnix: number;
  toUnix: number;
  afterUnix?: number | undefined;
  limit?: number | undefined;
}

export interface CandleSource {
  read(query: CandleSourceQuery): Promise<CandlePage>;
}

export class ArrayCandleSource implements CandleSource {
  constructor(private readonly bars: CandleBarDto[]) {}

  read(query: CandleSourceQuery): Promise<CandlePage> {
    const limit = Math.min(query.limit ?? 1000, 5000);
    const fromIndex = this.bars.findIndex(
      (bar) =>
        bar.openTimeUnix >= query.fromUnix &&
        (query.afterUnix === undefined || bar.openTimeUnix > query.afterUnix),
    );
    if (fromIndex === -1) {
      return Promise.resolve({ bars: [], nextCursorUnix: null });
    }
    const page: CandleBarDto[] = [];
    for (let i = fromIndex; i < this.bars.length && page.length <= limit; i += 1) {
      const bar = this.bars[i];
      if (bar === undefined || bar.openTimeUnix >= query.toUnix) {
        break;
      }
      if (query.afterUnix !== undefined && bar.openTimeUnix <= query.afterUnix) {
        continue;
      }
      page.push(bar);
    }
    const hasMore = page.length > limit;
    const result = hasMore ? page.slice(0, limit) : page;
    const last = result.at(-1);
    return Promise.resolve({
      bars: result,
      nextCursorUnix: hasMore && last ? last.openTimeUnix : null,
    });
  }
}

interface ApiCandlesResponse {
  candles: {
    openTimeUnix: number;
    close: string;
    volume: string;
    quoteTurnover: string;
  }[];
  nextCursorUnix: number | null;
}

export class ApiCandleSource implements CandleSource {
  constructor(
    private readonly baseUrl: string,
    private readonly apiKey: string,
    private readonly instrumentId: number,
  ) {}

  async read(query: CandleSourceQuery): Promise<CandlePage> {
    const params = new URLSearchParams({
      instrumentId: String(this.instrumentId),
      fromUnix: String(query.fromUnix),
      toUnix: String(query.toUnix),
    });
    if (query.limit !== undefined) {
      params.set("limit", String(query.limit));
    }
    if (query.afterUnix !== undefined) {
      params.set("afterUnix", String(query.afterUnix));
    }
    const response = await fetch(`${this.baseUrl}/v1/candles?${params.toString()}`, {
      headers: { "x-api-key": this.apiKey },
    });
    if (!response.ok) {
      throw new Error(
        `market data api ${response.status}: ${(await response.text()).slice(0, 200)}`,
      );
    }
    const body = (await response.json()) as ApiCandlesResponse;
    return {
      bars: body.candles.map((candle) => ({
        openTimeUnix: candle.openTimeUnix,
        close: candle.close,
        volume: candle.volume,
        quoteTurnover: candle.quoteTurnover,
      })),
      nextCursorUnix: body.nextCursorUnix,
    };
  }
}
