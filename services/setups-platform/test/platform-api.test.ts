 
import { afterAll, beforeAll, beforeEach, describe, expect, it } from "vitest";
import Fastify, { type FastifyInstance } from "fastify";
import type { Kysely } from "kysely";
import { createDb } from "../src/db/db.js";
import { createMigrator } from "../src/db/migrator.js";
import type { SignalsDB } from "../src/db/schema.js";
import { buildApp } from "../src/app.js";
import { ArrayCandleSource, type CandleBarDto } from "../src/replay/source.js";
import { liveScanOnce } from "../src/live/live-scanner.js";
import { DEFAULT_SPIKE_PARAMS } from "../src/detector/price-volume-spike.js";

const TEST_DATABASE_URL = process.env.TEST_DATABASE_URL;
const PLATFORM_KEY = "platform-key";

const MINUTE = 60;
const LONG_MINUTES = 30 * 1440;
const T0_MS = Date.UTC(2024, 5, 1);
const T0 = Math.floor(T0_MS / 1000);

const INSTRUMENT_ID = 7;
const SYMBOL = "E2EUSDT";

function buildBars(): CandleBarDto[] {
  const bars: CandleBarDto[] = [];
  for (let i = 0; i < LONG_MINUTES; i += 1) {
    bars.push({
      openTimeUnix: T0 + i * MINUTE,
      close: "100",
      volume: "100",
      quoteTurnover: "100",
    });
  }
  for (let i = LONG_MINUTES; i < LONG_MINUTES + 5; i += 1) {
    bars.push({
      openTimeUnix: T0 + i * MINUTE,
      close: "112",
      volume: "301",
      quoteTurnover: "301",
    });
  }
  for (let i = LONG_MINUTES + 5; i < LONG_MINUTES + 10; i += 1) {
    bars.push({
      openTimeUnix: T0 + i * MINUTE,
      close: "100",
      volume: "100",
      quoteTurnover: "100",
    });
  }
  return bars;
}

async function startFakeMarketData(bars: CandleBarDto[]): Promise<{
  app: FastifyInstance;
  url: string;
}> {
  const app = Fastify({ logger: false });
  app.get("/v1/instruments", () => ({
    instruments: [
      {
        id: INSTRUMENT_ID,
        exchange: "bybit",
        category: "linear",
        symbol: SYMBOL,
        status: "active",
      },
    ],
  }));
  app.get("/v1/candles", (request) => {
    const query = request.query as {
      instrumentId: string;
      fromUnix: string;
      toUnix: string;
      afterUnix?: string;
      limit?: string;
    };
    const from = Number(query.fromUnix);
    const to = Number(query.toUnix);
    const after = query.afterUnix !== undefined ? Number(query.afterUnix) : undefined;
    const limit = Math.min(Number(query.limit ?? 1000), 5000);
    const filtered = bars.filter(
      (bar) =>
        bar.openTimeUnix >= from &&
        bar.openTimeUnix < to &&
        (after === undefined || bar.openTimeUnix > after),
    );
    const page = filtered.slice(0, limit + 1);
    const hasMore = page.length > limit;
    const result = hasMore ? page.slice(0, limit) : page;
    const last = result.at(-1);
    return {
      candles: result.map((bar) => ({
        instrumentId: INSTRUMENT_ID,
        openTimeUnix: bar.openTimeUnix,
        open: bar.close,
        high: bar.close,
        low: bar.close,
        close: bar.close,
        volume: bar.volume,
        quoteTurnover: bar.quoteTurnover,
        source: "bybit",
      })),
      nextCursorUnix: hasMore && last ? last.openTimeUnix : null,
    };
  });
  const address = await app.listen({ port: 0, host: "127.0.0.1" });
  return { app, url: address };
}

describe.skipIf(!TEST_DATABASE_URL)("platform api", () => {
  let db: Kysely<SignalsDB>;
  let app: FastifyInstance;
  let marketData: FastifyInstance;
  const bars = buildBars();
  const fromUnix = T0 + LONG_MINUTES * MINUTE;
  const toUnix = T0 + (LONG_MINUTES + 10) * MINUTE;

  beforeAll(async () => {
    db = createDb(TEST_DATABASE_URL!);
    const migrator = createMigrator(db);
    const migrationResult = await migrator.migrateToLatest();
    if (migrationResult.error) {
      throw new Error(`migration failed: ${JSON.stringify(migrationResult.error)}`);
    }
    const fake = await startFakeMarketData(bars);
    marketData = fake.app;
    app = await buildApp({
      db,
      apiKeys: [PLATFORM_KEY],
      openApiEnabled: false,
      marketData: { url: fake.url, apiKey: "fake" },
    });
  });

  afterAll(async () => {
    await app.close();
    await marketData.close();
    await db.destroy();
  });

  beforeEach(async () => {
    await db.deleteFrom("replay_cache").execute();
    await db.deleteFrom("replay_runs").execute();
    await db.deleteFrom("live_setups").execute();
    await db.deleteFrom("settings").execute();
  });

  it("requires an api key", async () => {
    const response = await app.inject({ method: "GET", url: "/v1/replay-runs" });
    expect(response.statusCode).toBe(401);
  });

  it("computes a replay end to end and reuses the cache on repeat", async () => {
    const first = await app.inject({
      method: "POST",
      url: "/v1/replay-runs",
      headers: { "x-api-key": PLATFORM_KEY },
      payload: {
        instruments: [{ exchange: "bybit", category: "linear", symbol: SYMBOL }],
        fromUnix,
        toUnix,
      },
    });
    expect(first.statusCode).toBe(200);
    const firstBody = first.json<{
      runs: { id: number; status: string; cacheHit: boolean; setupCount: number }[];
    }>();
    expect(firstBody.runs).toHaveLength(1);
    expect(firstBody.runs[0]?.status).toBe("done");
    expect(firstBody.runs[0]?.cacheHit).toBe(false);
    expect(firstBody.runs[0]?.setupCount).toBeGreaterThanOrEqual(1);

    const second = await app.inject({
      method: "POST",
      url: "/v1/replay-runs",
      headers: { "x-api-key": PLATFORM_KEY },
      payload: {
        instruments: [{ exchange: "bybit", category: "linear", symbol: SYMBOL }],
        fromUnix,
        toUnix,
      },
    });
    const secondBody = second.json<{
      runs: { status: string; cacheHit: boolean; setupCount: number }[];
    }>();
    expect(secondBody.runs[0]?.status).toBe("done");
    expect(secondBody.runs[0]?.cacheHit).toBe(true);
    expect(secondBody.runs[0]?.setupCount).toBe(firstBody.runs[0]?.setupCount);
  });

  it("exposes run details, list, and setups export", async () => {
    const created = await app.inject({
      method: "POST",
      url: "/v1/replay-runs",
      headers: { "x-api-key": PLATFORM_KEY },
      payload: {
        instruments: [{ exchange: "bybit", category: "linear", symbol: SYMBOL }],
        fromUnix,
        toUnix,
      },
    });
    const createdBody = created.json<{ runs: { id: number }[] }>();
    const runId = createdBody.runs[0]?.id ?? 0;

    const detail = await app.inject({
      method: "GET",
      url: `/v1/replay-runs/${runId}`,
      headers: { "x-api-key": PLATFORM_KEY },
    });
    const detailBody = detail.json<{
      run: { status: string };
      setups: { symbol: string; closePrice: string; openTimeUnix: number }[];
    }>();
    expect(detailBody.run.status).toBe("done");
    expect(detailBody.setups.length).toBeGreaterThanOrEqual(1);
    expect(detailBody.setups[0]?.symbol).toBe(SYMBOL);
    expect(detailBody.setups[0]?.closePrice).toBe("112");
    expect(detailBody.setups[0]?.openTimeUnix).toBe(T0 + (LONG_MINUTES + 4) * MINUTE);

    const list = await app.inject({
      method: "GET",
      url: "/v1/replay-runs",
      headers: { "x-api-key": PLATFORM_KEY },
    });
    expect(list.json<{ runs: unknown[] }>().runs.length).toBeGreaterThanOrEqual(1);

    const setups = await app.inject({
      method: "GET",
      url: "/v1/setups",
      headers: { "x-api-key": PLATFORM_KEY },
    });
    expect(setups.json<{ setups: unknown[] }>().setups).toHaveLength(0);

    const csv = await app.inject({
      method: "GET",
      url: "/v1/export/setups.csv",
      headers: { "x-api-key": PLATFORM_KEY },
    });
    expect(csv.statusCode).toBe(200);
    expect(csv.body.split("\n")[0]).toContain("exchange");
  });

  it("stores and returns the notification age setting", async () => {
    const updated = await app.inject({
      method: "PUT",
      url: "/v1/settings/notifications",
      headers: { "x-api-key": PLATFORM_KEY },
      payload: { maxAgeSeconds: 120 },
    });
    expect(updated.json<{ maxAgeSeconds: number }>().maxAgeSeconds).toBe(120);
    const read = await app.inject({
      method: "GET",
      url: "/v1/settings/notifications",
      headers: { "x-api-key": PLATFORM_KEY },
    });
    expect(read.json<{ maxAgeSeconds: number }>().maxAgeSeconds).toBe(120);
  });

  it("clears the cache by instrument and reports stats", async () => {
    await app.inject({
      method: "POST",
      url: "/v1/replay-runs",
      headers: { "x-api-key": PLATFORM_KEY },
      payload: {
        instruments: [{ exchange: "bybit", category: "linear", symbol: SYMBOL }],
        fromUnix,
        toUnix,
      },
    });
    const stats = await app.inject({
      method: "GET",
      url: "/v1/replay-cache/stats",
      headers: { "x-api-key": PLATFORM_KEY },
    });
    expect(stats.json<{ entries: number }>().entries).toBe(1);

    const cleared = await app.inject({
      method: "DELETE",
      url: `/v1/replay-cache/bybit/linear/${SYMBOL}`,
      headers: { "x-api-key": PLATFORM_KEY },
    });
    expect(cleared.json<{ removed: number }>().removed).toBe(1);
    const after = await app.inject({
      method: "GET",
      url: "/v1/replay-cache/stats",
      headers: { "x-api-key": PLATFORM_KEY },
    });
    expect(after.json<{ entries: number }>().entries).toBe(0);
  });

  it("live scan stores setups once and notifies only fresh signals", async () => {
    const source = new ArrayCandleSource(bars);
    const nowUnix = T0 + (LONG_MINUTES + 5) * MINUTE + 30;
    const notifier = { kind: "test", send: () => Promise.resolve() };
    const first = await liveScanOnce({
      db,
      source,
      instruments: [{ exchangeCode: "bybit", category: "linear", symbol: SYMBOL }],
      nowUnix,
      maxAgeSeconds: 60,
      notifier,
    });
    expect(first.newSetups.length).toBeGreaterThanOrEqual(1);
    expect(first.notified).toBeGreaterThanOrEqual(1);

    const second = await liveScanOnce({
      db,
      source,
      instruments: [{ exchangeCode: "bybit", category: "linear", symbol: SYMBOL }],
      nowUnix,
      maxAgeSeconds: 60,
      notifier,
    });
    expect(second.newSetups).toHaveLength(0);
    expect(second.notified).toBe(0);

    const setups = await db.selectFrom("live_setups").selectAll().execute();
    expect(setups.length).toBeGreaterThanOrEqual(1);
    expect(setups[0]?.params).toEqual(DEFAULT_SPIKE_PARAMS);
  });
});
