 
import { afterAll, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import Fastify, { type FastifyInstance } from "fastify";
import type { Kysely } from "kysely";
import { createDb } from "../src/db/db.js";
import { createMigrator } from "../src/db/migrator.js";
import type { SignalsDB } from "../src/db/schema.js";
import { buildApp } from "../src/app.js";
import { ArrayCandleSource, type CandleBarDto } from "../src/replay/source.js";
import { liveScanOnce } from "../src/live/live-scanner.js";
import { DEFAULT_SPIKE_PARAMS, type SpikeSetup } from "../src/detector/price-volume-spike.js";
import { startLiveRuntime } from "../src/live/runtime.js";
import { getSetting, setSetting } from "../src/settings.js";

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
    await db.deleteFrom("replay_batches").execute();
    await db.deleteFrom("live_setups").execute();
    await db.deleteFrom("settings").execute();
    await db.deleteFrom("instrument_lists").execute();
  });

  it("requires an api key", async () => {
    const response = await app.inject({ method: "GET", url: "/v1/replay-runs" });
    expect(response.statusCode).toBe(401);
  });

  it("purges replay runs via the API and reports active-run conflicts", async () => {
    const headers = { "x-api-key": PLATFORM_KEY };
    await db.deleteFrom("replay_runs").execute();
    const seed = { exchange_code: "bybit", category: "linear", symbol: "PURGEUSDT", from_ts: new Date(0), to_ts: new Date(60_000), algorithm_id: "price_volume_spike", algorithm_version: "1", params: DEFAULT_SPIKE_PARAMS, cache_hit: false, setups: [] };
    await db.insertInto("replay_runs").values({ ...seed, status: "done" }).execute();
    const purged = await app.inject({ method: "POST", url: "/v1/replay-runs/purge", headers });
    expect(purged.statusCode).toBe(200);
    expect(purged.json<{ runsDeleted: number }>().runsDeleted).toBeGreaterThanOrEqual(1);
    await db.insertInto("replay_runs").values({ ...seed, status: "running" }).execute();
    const conflict = await app.inject({ method: "POST", url: "/v1/replay-runs/purge", headers });
    expect(conflict.statusCode).toBe(409);
    expect(conflict.json<{ error: string; active: number }>()).toMatchObject({ error: "active_runs", active: 1 });
    await db.deleteFrom("replay_runs").execute();
  });

  it("serves batch grouping, unified detail, cancel and CSV via the API", async () => {
    const headers = { "x-api-key": PLATFORM_KEY };
    await db.deleteFrom("replay_batches").execute();
    const batch = await db.insertInto("replay_batches").values({ exchange_code: "bybit", category: "linear", algorithm_id: "price_volume_spike", algorithm_version: "1", params: DEFAULT_SPIKE_PARAMS, from_ts: new Date(0), to_ts: new Date(60_000), instrument_count: 2 }).returning("id").executeTakeFirstOrThrow();
    const setup = { algorithmId: "price_volume_spike", algorithmVersion: "1", openTimeUnix: 60, closeTimeUnix: 120, closePrice: "101", params: DEFAULT_SPIKE_PARAMS };
    const seed = { exchange_code: "bybit", category: "linear", from_ts: new Date(0), to_ts: new Date(60_000), algorithm_id: "price_volume_spike", algorithm_version: "1", params: DEFAULT_SPIKE_PARAMS, cache_hit: false, batch_id: batch.id };
    await db.insertInto("replay_runs").values([
      { ...seed, symbol: "BATCHAAUSDT", status: "done", setups: JSON.stringify([setup]) as unknown as SpikeSetup[] },
      { ...seed, symbol: "BATCHBBUSDT", status: "queued", setups: JSON.stringify([]) as unknown as SpikeSetup[] },
    ]).execute();
    const list = await app.inject({ method: "GET", url: "/v1/replay-runs", headers });
    const listBody = list.json<{ runs: { id: number; isBatch?: boolean; symbol: string; status: string; setupCount: number }[] }>();
    const row = listBody.runs.find((item) => item.isBatch === true);
    expect(row).toMatchObject({ symbol: "2 инструментов", status: "running", setupCount: 1 });
    expect(listBody.runs.some((item) => item.symbol === "BATCHAAUSDT")).toBe(false);
    const detail = await app.inject({ method: "GET", url: `/v1/replay-batches/${batch.id}?offset=0&limit=10`, headers });
    expect(detail.statusCode).toBe(200);
    const detailBody = detail.json<{ run: { instrumentCount: number }; setups: { symbol: string }[]; total: number }>();
    expect(detailBody.run.instrumentCount).toBe(2);
    expect(detailBody.total).toBe(1);
    expect(detailBody.setups.map((item) => item.symbol)).toEqual(["BATCHAAUSDT"]);
    const cancel = await app.inject({ method: "POST", url: `/v1/replay-batches/${batch.id}/cancel`, headers });
    expect(cancel.statusCode).toBe(200);
    expect(cancel.json()).toEqual({ cancelled: 1, cancelRequested: 0 });
    const csv = await app.inject({ method: "GET", url: `/v1/replay-batches/${batch.id}/export.csv`, headers });
    expect(csv.statusCode).toBe(200);
    expect(csv.body).toContain("BATCHAAUSDT");
    await db.deleteFrom("replay_runs").execute();
    await db.deleteFrom("replay_batches").execute();
  });

  it("persists shared algorithm and UI settings and snapshots them in replay", async () => {
    const headers = { "x-api-key": PLATFORM_KEY };
    const algorithms = await app.inject({ method: "GET", url: "/v1/algorithms", headers });
    expect(algorithms.statusCode).toBe(200);
    const params = { ...DEFAULT_SPIKE_PARAMS, priceGrowthPct: "12.5", volumeMultiplier: "2.5" };
    const saved = await app.inject({ method: "PUT", url: "/v1/algorithms/price_volume_spike", headers, payload: { params, enabled: false } });
    expect(saved.statusCode).toBe(200);
    expect(saved.json<{ params: typeof params }>().params).toEqual(params);
    const ui = { replay: { symbols: SYMBOL, algorithmId: "price_volume_spike" }, setupsSymbol: SYMBOL };
    expect((await app.inject({ method: "PUT", url: "/v1/settings/ui", headers, payload: { value: ui } })).statusCode).toBe(200);
    expect((await app.inject({ method: "GET", url: "/v1/settings/ui", headers })).json()).toEqual({ value: ui });
    const response = await app.inject({ method: "POST", url: "/v1/replay-runs", headers, payload: {
      instruments: [{ exchange: "bybit", category: "linear", symbol: SYMBOL }], fromUnix, toUnix, algorithmId: "price_volume_spike",
    } });
    expect(response.statusCode).toBe(200);
    const id = response.json<{ runs: { id: number }[] }>().runs[0]!.id;
    const detail = (await app.inject({ method: "GET", url: `/v1/replay-runs/${id}`, headers })).json<{ run: { params: typeof params; algorithmId: string } }>();
    expect(detail.run.params).toEqual(params);
    expect(detail.run.algorithmId).toBe("price_volume_spike");
    await setSetting(db, "webpush-vapid", { privateKey: "must-not-export" });
    const exported = await app.inject({ method: "GET", url: "/v1/export/config.json", headers });
    expect(exported.statusCode).toBe(200);
    expect(exported.body).not.toContain("must-not-export");
    expect(exported.json<{ algorithms: { params: typeof params }[] }>().algorithms[0]!.params).toEqual(params);
    const reopened = await buildApp({ db, apiKeys: [PLATFORM_KEY], openApiEnabled: false });
    expect((await reopened.inject({ method: "GET", url: "/v1/settings/ui", headers })).json()).toEqual({ value: ui });
    await reopened.close();
  });

  it("rejects unknown algorithms, invalid ranges and unsafe parameter combinations", async () => {
    const headers = { "x-api-key": PLATFORM_KEY };
    for (const params of [
      { ...DEFAULT_SPIKE_PARAMS, baselineShortDays: 31 },
      { ...DEFAULT_SPIKE_PARAMS, volumeMultiplier: "0" },
      { ...DEFAULT_SPIKE_PARAMS, maxGapPercent: "101" },
      { ...DEFAULT_SPIKE_PARAMS, windowMinutes: -1 },
      { ...DEFAULT_SPIKE_PARAMS, priceGrowthPct: "NaN" },
    ]) {
      expect((await app.inject({ method: "PUT", url: "/v1/algorithms/price_volume_spike", headers, payload: { params, enabled: true } })).statusCode).toBe(400);
    }
    const payload = { instruments: [{ exchange: "bybit", category: "linear", symbol: SYMBOL }], fromUnix, toUnix };
    expect((await app.inject({ method: "POST", url: "/v1/replay-runs", headers, payload: { ...payload, algorithmId: "unknown" } })).statusCode).toBe(400);
    expect((await app.inject({ method: "POST", url: "/v1/replay-runs", headers, payload: { ...payload, toUnix: fromUnix } })).statusCode).toBe(400);
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

  it("restores live cooldown across consecutive scans and restart", async () => {
    const params = { ...DEFAULT_SPIKE_PARAMS, baselineLongDays: 1, baselineShortDays: 1, priceGrowthPct: "0", volumeMultiplier: "0.5" };
    await setSetting(db, "algorithm:price_volume_spike", { params, enabled: true });
    const data = Array.from({ length: 1450 }, (_, i) => ({ openTimeUnix: T0 + i * 60, close: "100", volume: "100", quoteTurnover: "100" }));
    const source = new ArrayCandleSource(data);
    const instruments = [{ exchangeCode: "bybit", category: "linear", symbol: SYMBOL }];
    const first = await liveScanOnce({ db, source, instruments, nowUnix: T0 + 1440 * 60, maxAgeSeconds: 60 });
    expect(first.newSetups).toHaveLength(1);
    const second = await liveScanOnce({ db, source: new ArrayCandleSource(data), instruments, nowUnix: T0 + 1441 * 60, maxAgeSeconds: 60 });
    expect(second.newSetups).toHaveLength(0);
    const next = await liveScanOnce({ db, source, instruments, nowUnix: T0 + 1445 * 60, maxAgeSeconds: 60 });
    expect(next.newSetups).toHaveLength(1);
    expect(next.newSetups[0]!.openTimeUnix - first.newSetups[0]!.openTimeUnix).toBe(300);
  });

  it("runtime catches up delayed closed candles without enabling itself or duplicating signals", async () => {
    const delayedBars = buildBars().slice(0, LONG_MINUTES + 4);
    const fake = await startFakeMarketData(delayedBars);
    const runtimeApp = Fastify({ logger: false });
    const runtime = startLiveRuntime(runtimeApp, db, { url: fake.url, apiKey: "fake" });
    const time = vi.spyOn(Date, "now").mockReturnValue((T0 + (LONG_MINUTES + 6) * 60) * 1000);
    try {
      await runtime.runOnce();
      expect(await getSetting(db, "live-status")).toEqual({});
      await setSetting(db, "live", { enabled: true });
      await runtime.runOnce();
      expect((await db.selectFrom("live_setups").selectAll().execute())).toHaveLength(0);
      delayedBars.push(buildBars()[LONG_MINUTES + 4]!);
      time.mockReturnValue((T0 + (LONG_MINUTES + 7) * 60) * 1000);
      await runtime.runOnce();
      expect((await db.selectFrom("live_setups").selectAll().execute())).toHaveLength(1);
      time.mockReturnValue((T0 + (LONG_MINUTES + 8) * 60) * 1000);
      await runtime.runOnce();
      expect((await db.selectFrom("live_setups").selectAll().execute())).toHaveLength(1);
      expect((await getSetting(db, "live-status")).stale).toBe(1);
    } finally {
      time.mockRestore();
      await runtimeApp.close();
      await fake.app.close();
    }
  });
});
