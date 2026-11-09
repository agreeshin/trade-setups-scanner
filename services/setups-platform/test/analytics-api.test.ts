import { afterAll, beforeAll, beforeEach, describe, expect, it } from "vitest";
import Fastify, { type FastifyInstance } from "fastify";
import type { Kysely } from "kysely";
import { createDb } from "../src/db/db.js";
import { createMigrator } from "../src/db/migrator.js";
import type { SignalsDB } from "../src/db/schema.js";
import { buildApp } from "../src/app.js";
import { DEFAULT_SPIKE_PARAMS, type SpikeSetup } from "../src/detector/price-volume-spike.js";
import { resetCandleWindowCache } from "../src/analytics/candles.js";

const TEST_DATABASE_URL = process.env.TEST_DATABASE_URL;
const PLATFORM_KEY = "platform-key";

const MINUTE = 60;
const HOUR = 60 * MINUTE;
const DAY = 24 * HOUR;
const T0 = Math.floor(Date.UTC(2024, 5, 1) / 1000);

const INSTRUMENT_ID = 41;
const SYMBOL = "ANALYTICSUSDT";

interface FakeCandle {
  openTimeUnix: number;
  open: string;
  high: string;
  low: string;
  close: string;
}

/** Flat candles at 100 with two spike candles for TP (after setup 1) and SL (after setup 2). */
function buildCandles(): FakeCandle[] {
  const candles: FakeCandle[] = [];
  const end = T0 + 45 * DAY;
  const flat = (time: number): FakeCandle => ({ openTimeUnix: time, open: "100", high: "100", low: "100", close: "100" });
  for (let time = T0; time < end; time += MINUTE) {
    candles.push(flat(time));
  }
  const override = (time: number, high: string, low: string): void => {
    const index = candles.findIndex((candle) => candle.openTimeUnix === time);
    if (index >= 0) candles[index] = { ...candles[index]!, high, low };
  };
  // Setup 1 entry candle T0 (shift 0): TP=96/SL=104 touch three minutes later.
  override(T0 + 3 * MINUTE, "103", "95");
  // Setup 2 entry candle T0+1h: SL=104 touch two minutes later.
  override(T0 + HOUR + 2 * MINUTE, "105", "99");
  return candles;
}

function makeSetup(openTimeUnix: number, closePrice: string): SpikeSetup {
  return {
    algorithmId: "price_volume_spike",
    algorithmVersion: "1.1.1",
    openTimeUnix,
    closeTimeUnix: openTimeUnix + MINUTE,
    closePrice,
    params: DEFAULT_SPIKE_PARAMS,
  };
}

/** June cluster of three setups (gaps <= 8h) plus one July setup in its own cluster. */
const SETUPS: SpikeSetup[] = [
  makeSetup(T0, "100"), // entry close 100 -> TP at +3m
  makeSetup(T0 + HOUR, "100"), // entry close 100 -> SL at +2m
  makeSetup(T0 + 2 * HOUR, "100"), // flat -> unclosed
  makeSetup(T0 + 40 * DAY, "100"), // July, flat -> unclosed
];

async function startFakeMarketData(candles: FakeCandle[]): Promise<{ app: FastifyInstance; url: string }> {
  const app = Fastify({ logger: false });
  app.get("/v1/instruments", () => ({
    instruments: [{ id: INSTRUMENT_ID, exchange: "bybit", category: "linear", symbol: SYMBOL, status: "active" }],
  }));
  app.get("/v1/candles", (request) => {
    const query = request.query as { instrumentId: string; fromUnix: string; toUnix: string; afterUnix?: string; limit?: string };
    const from = Number(query.fromUnix);
    const to = Number(query.toUnix);
    const after = query.afterUnix !== undefined ? Number(query.afterUnix) : undefined;
    const limit = Math.min(Number(query.limit ?? 1000), 5000);
    const filtered = candles.filter(
      (candle) => candle.openTimeUnix >= from && candle.openTimeUnix < to && (after === undefined || candle.openTimeUnix > after),
    );
    const page = filtered.slice(0, limit);
    const last = page.at(-1);
    return {
      candles: page.map((candle) => ({ instrumentId: INSTRUMENT_ID, ...candle, volume: "1", quoteTurnover: "1", source: "bybit" })),
      nextCursorUnix: last === undefined || page.length < limit ? null : last.openTimeUnix,
    };
  });
  const address = await app.listen({ port: 0, host: "127.0.0.1" });
  return { app, url: address };
}

interface SetSummary {
  setId: string;
  label: string;
  setupCount: number;
  kind: string;
  params: Record<string, string | number>;
}

interface MatrixCellBody {
  cached: boolean;
  cachedSelection: boolean;
  closedCount: number;
  tpCount: number;
  slCount: number;
  unclosedCount: number;
  avgMovePct: number | null;
  riskExpectancy: number | null;
  winRatePct: number | null;
  totalMatchingSetups: number;
  filteredSetupsCount: number;
  eligibleSetupsCount: number;
  priceChangeFilteredCount: number;
  avgSelectionOrdinal: number | null;
  avgSelectionDelayMinutes: number | null;
  totalClustersCount: number | null;
}

interface TradeRowBody {
  cached: boolean;
  status: string;
  entryPrice: number | null;
  tpPrice: number | null;
  slPrice: number | null;
  closeTimeUnix: number | null;
  riskReturn: number | null;
}

describe.skipIf(!TEST_DATABASE_URL)("analytics api", () => {
  let db: Kysely<SignalsDB>;
  let app: FastifyInstance;
  let marketData: FastifyInstance;
  let runId: number;

  beforeAll(async () => {
    db = createDb(TEST_DATABASE_URL!);
    const migrator = createMigrator(db);
    const migration = await migrator.migrateToLatest();
    if (migration.error) throw new Error(`migration failed: ${JSON.stringify(migration.error)}`);
    const fake = await startFakeMarketData(buildCandles());
    marketData = fake.app;
    app = await buildApp({
      db,
      apiKeys: [PLATFORM_KEY],
      marketData: { url: fake.url, apiKey: "market-key" },
      openApiEnabled: false,
    });
  });

  afterAll(async () => {
    await app.close();
    await marketData.close();
    await db.destroy();
    resetCandleWindowCache();
  });

  beforeEach(async () => {
    await db.deleteFrom("analytics_matrix_cache").execute();
    await db.deleteFrom("analytics_trades_cache").execute();
    await db.deleteFrom("analytics_selection_cache").execute();
    await db.deleteFrom("replay_runs").execute();
    await db.deleteFrom("replay_batches").execute();
    resetCandleWindowCache();
    const inserted = await db
      .insertInto("replay_runs")
      .values({
        exchange_code: "bybit",
        category: "linear",
        symbol: SYMBOL,
        from_ts: new Date((T0 - 60) * 1000),
        to_ts: new Date((T0 + 41 * DAY) * 1000),
        algorithm_id: "price_volume_spike",
        algorithm_version: "1.1.1",
        params: DEFAULT_SPIKE_PARAMS,
        status: "done",
        cache_hit: false,
        setups: JSON.stringify(SETUPS) as unknown as SpikeSetup[],
        warmup_minutes: 43200,
        processed_minutes: 60,
      })
      .returning("id")
      .executeTakeFirst();
    runId = inserted!.id;
  });

  const auth = { "x-api-key": PLATFORM_KEY } as const;
  const base = "/v1/analytics";

  function url(path: string, params: Record<string, string | number | undefined>): string {
    const search = new URLSearchParams();
    for (const [key, value] of Object.entries(params)) {
      if (value !== undefined) search.set(key, String(value));
    }
    return `${base}${path}?${search.toString()}`;
  }

  it("lists sets with the selector label", async () => {
    const response = await app.inject({ method: "GET", url: `${base}/sets`, headers: auth });
    expect(response.statusCode).toBe(200);
    const body = response.json<{ sets: SetSummary[] }>();
    const set = body.sets.find((item) => item.setId === `run-${runId}`);
    expect(set).toBeDefined();
    expect(set!.label).toBe(`#${runId} - Всплеск цены и оборота · 1.1.1`);
    expect(set!.setupCount).toBe(4);
    expect(set!.params).toEqual(DEFAULT_SPIKE_PARAMS);
  });

  it("returns setup months", async () => {
    const response = await app.inject({
      method: "GET",
      url: url("/months", { set: `run-${runId}` }),
      headers: auth,
    });
    expect(response.statusCode).toBe(200);
    expect(response.json<{ months: string[] }>()).toEqual({ months: ["2024-06", "2024-07"] });
  });

  it("lists set setups with maturity selection", async () => {
    const plain = await app.inject({
      method: "GET", url: url("/setups", { set: `run-${runId}` }), headers: auth,
    });
    expect(plain.statusCode).toBe(200);
    const plainBody = plain.json<{ setups: { symbol: string; closeTimeUnix: number }[]; totalMatchingSetups: number; totalClustersCount: number | null }>();
    expect(plainBody.setups).toHaveLength(4);
    expect(plainBody.totalClustersCount).toBeNull();

    const maturity = await app.inject({
      method: "GET", url: url("/setups", { set: `run-${runId}`, setupMaturity: 0.5, month: "2024-06" }), headers: auth,
    });
    expect(maturity.statusCode).toBe(200);
    const maturityBody = maturity.json<{ setups: { symbol: string; closeTimeUnix: number }[]; totalMatchingSetups: number; totalClustersCount: number | null; avgSelectionOrdinal: number | null }>();
    expect(maturityBody.setups).toHaveLength(1);
    expect(maturityBody.setups[0]!.closeTimeUnix).toBe(T0 + HOUR + MINUTE);
    expect(maturityBody.totalMatchingSetups).toBe(3);
    expect(maturityBody.totalClustersCount).toBe(1);
    expect(maturityBody.avgSelectionOrdinal).toBe(2);
  });

  it("computes a short matrix cell and serves it from cache on repeat", async () => {
    const cellUrl = url("/matrix-cell", { set: `run-${runId}`, direction: "short", tpPct: 4, slPct: 4, horizonMinutes: 60 });
    const first = await app.inject({ method: "GET", url: cellUrl, headers: auth });
    expect(first.statusCode).toBe(200);
    const firstBody = first.json<MatrixCellBody>();
    expect(firstBody.cached).toBe(false);
    expect(firstBody.closedCount).toBe(2);
    expect(firstBody.tpCount).toBe(1);
    expect(firstBody.slCount).toBe(1);
    expect(firstBody.unclosedCount).toBe(2);
    expect(firstBody.avgMovePct).toBe(0);
    expect(firstBody.riskExpectancy).toBe(0);
    expect(firstBody.winRatePct).toBe(50);
    expect(firstBody.totalMatchingSetups).toBe(4);

    const second = await app.inject({ method: "GET", url: cellUrl, headers: auth });
    expect(second.statusCode).toBe(200);
    const secondBody = second.json<MatrixCellBody>();
    expect(secondBody.cached).toBe(true);
    expect(secondBody.closedCount).toBe(2);

    // Different TP/SL is a different cache key.
    const other = await app.inject({
      method: "GET", url: url("/matrix-cell", { set: `run-${runId}`, direction: "short", tpPct: 6, slPct: 4, horizonMinutes: 60 }),
      headers: auth,
    });
    expect(other.json<MatrixCellBody>().cached).toBe(false);
  });

  it("applies maturity clustering and month filters", async () => {
    const maturity = await app.inject({
      method: "GET",
      url: url("/matrix-cell", {
        set: `run-${runId}`, direction: "short", tpPct: 4, slPct: 4, horizonMinutes: 60, setupMaturity: 0.5,
      }),
      headers: auth,
    });
    expect(maturity.statusCode).toBe(200);
    const body = maturity.json<MatrixCellBody>();
    // Two clusters (June trio + July single); 0.5 picks the middle of the trio and the July setup.
    expect(body.filteredSetupsCount).toBe(2);
    expect(body.avgSelectionOrdinal).toBe(1.5); // (2 + 1) / 2
    expect(body.totalClustersCount).toBe(2);
    expect(body.slCount).toBe(1); // middle setup closes by SL, July setup unclosed
    expect(body.unclosedCount).toBe(1);
    expect(body.riskExpectancy).toBe(-1);

    const june = await app.inject({
      method: "GET",
      url: url("/matrix-cell", {
        set: `run-${runId}`, direction: "short", tpPct: 4, slPct: 4, horizonMinutes: 60,
        setupMaturity: 0.5, month: "2024-06",
      }),
      headers: auth,
    });
    expect(june.statusCode).toBe(200);
    const juneBody = june.json<MatrixCellBody>();
    // Month filter applies before clustering: only the June trio, middle setup selected.
    expect(juneBody.totalMatchingSetups).toBe(3);
    expect(juneBody.filteredSetupsCount).toBe(1);
    expect(juneBody.slCount).toBe(1);
    expect(juneBody.totalClustersCount).toBe(1);
  });

  it("rejects invalid maturity granularity and unknown sets", async () => {
    const badMaturity = await app.inject({
      method: "GET", url: url("/matrix-cell", { set: `run-${runId}`, tpPct: 4, slPct: 4, setupMaturity: 0.15 }), headers: auth,
    });
    expect(badMaturity.statusCode).toBe(400);

    const unknownSet = await app.inject({
      method: "GET", url: url("/matrix-cell", { set: "run-99999", tpPct: 4, slPct: 4 }), headers: auth,
    });
    expect(unknownSet.statusCode).toBe(400);
  });

  it("separates matrix cache entries by the cluster gap", async () => {
    const common = { set: `run-${runId}`, direction: "short" as const, tpPct: 7, slPct: 9, horizonMinutes: 60, setupMaturity: 0.5 };
    const first = await app.inject({ method: "GET", url: url("/matrix-cell", { ...common, clusterGapHours: 2 }), headers: auth });
    expect(first.statusCode).toBe(200);
    expect(first.json<MatrixCellBody>().cached).toBe(false);
    const repeat = await app.inject({ method: "GET", url: url("/matrix-cell", { ...common, clusterGapHours: 2 }), headers: auth });
    expect(repeat.json<MatrixCellBody>().cached).toBe(true);
    const otherGap = await app.inject({ method: "GET", url: url("/matrix-cell", { ...common, clusterGapHours: 5 }), headers: auth });
    expect(otherGap.statusCode).toBe(200);
    expect(otherGap.json<MatrixCellBody>().cached).toBe(false);
  });

  it("caches no-clustering cells the same way as clustered ones", async () => {
    const cellUrl = url("/matrix-cell", { set: `run-${runId}`, direction: "short", tpPct: 8, slPct: 10, horizonMinutes: 60 });
    const first = await app.inject({ method: "GET", url: cellUrl, headers: auth });
    expect(first.statusCode).toBe(200);
    const firstBody = first.json<MatrixCellBody & { totalClustersCount: number | null; avgSelectionOrdinal: number | null }>();
    expect(firstBody.cached).toBe(false);
    expect(firstBody.totalClustersCount).toBeNull();
    expect(firstBody.avgSelectionOrdinal).toBeNull();
    const repeat = await app.inject({ method: "GET", url: cellUrl, headers: auth });
    expect(repeat.json<MatrixCellBody>().cached).toBe(true);
  });

  it("rejects an out-of-range cluster gap", async () => {
    const tooSmall = await app.inject({
      method: "GET", url: url("/matrix-cell", { set: `run-${runId}`, tpPct: 4, slPct: 4, clusterGapHours: 0.001 }), headers: auth,
    });
    expect(tooSmall.statusCode).toBe(400);

    const tooLarge = await app.inject({
      method: "GET", url: url("/matrix-cell", { set: `run-${runId}`, tpPct: 4, slPct: 4, clusterGapHours: 200 }), headers: auth,
    });
    expect(tooLarge.statusCode).toBe(400);
  });

  it("applies the price-change gate and reports filtered setups separately", async () => {
    const gated = await app.inject({
      method: "GET",
      url: url("/matrix-cell", {
        set: `run-${runId}`, direction: "short", tpPct: 4, slPct: 4, horizonMinutes: 60, priceChangePct: 1,
      }),
      headers: auth,
    });
    expect(gated.statusCode).toBe(200);
    const body = gated.json<MatrixCellBody>();
    // Flat candles: entry 100 never grew 1% above setup close 100 -> all filtered.
    expect(body.eligibleSetupsCount).toBe(0);
    expect(body.priceChangeFilteredCount).toBe(4);
    expect(body.closedCount).toBe(0);
    expect(body.riskExpectancy).toBeNull();

    const disabledGate = await app.inject({
      method: "GET", url: url("/matrix-cell", { set: `run-${runId}`, direction: "short", tpPct: 4, slPct: 4, horizonMinutes: 60 }),
      headers: auth,
    });
    expect(disabledGate.json<MatrixCellBody>().priceChangeFilteredCount).toBe(0);
  });

  it("computes a long trade row, caches it and lists it in bulk", async () => {
    const pointUrl = url(`/trades/${SYMBOL}/${SETUPS[0]!.closeTimeUnix}`, {
      set: `run-${runId}`, direction: "long", tpPct: 4, slPct: 4, horizonMinutes: 60,
    });
    const point = await app.inject({ method: "GET", url: pointUrl, headers: auth });
    expect(point.statusCode).toBe(200);
    const pointBody = point.json<TradeRowBody>();
    // Long TP=104 is never touched, but the spike candle (low 95) hits SL=96 first.
    expect(pointBody.status).toBe("sl");
    expect(pointBody.entryPrice).toBe(100);
    expect(pointBody.slPrice).toBeCloseTo(96);
    expect(pointBody.riskReturn).toBe(-1);
    expect(pointBody.cached).toBe(false);

    const cachedPoint = await app.inject({ method: "GET", url: pointUrl, headers: auth });
    expect(cachedPoint.json<TradeRowBody>().cached).toBe(true);

    const shortPoint = await app.inject({
      method: "GET",
      url: url(`/trades/${SYMBOL}/${SETUPS[0]!.closeTimeUnix}`, {
        set: `run-${runId}`, direction: "short", tpPct: 4, slPct: 4, horizonMinutes: 60,
      }),
      headers: auth,
    });
    const shortBody = shortPoint.json<TradeRowBody>();
    expect(shortBody.status).toBe("tp");
    expect(shortBody.tpPrice).toBeCloseTo(96);
    expect(shortBody.riskReturn).toBe(1);
    expect(shortBody.closeTimeUnix).toBe(T0 + 3 * MINUTE);

    const bulk = await app.inject({
      method: "GET",
      url: url("/trades", { set: `run-${runId}`, direction: "short", tpPct: 4, slPct: 4, horizonMinutes: 60 }),
      headers: auth,
    });
    expect(bulk.statusCode).toBe(200);
    const bulkBody = bulk.json<{ items: unknown[]; cachedCount: number }>();
    expect(bulkBody.cachedCount).toBe(1);
    expect(bulkBody.items).toHaveLength(1);

    const missing = await app.inject({
      method: "GET",
      url: url(`/trades/${SYMBOL}/1234567890`, { set: `run-${runId}`, direction: "long", tpPct: 4, slPct: 4, horizonMinutes: 60 }),
      headers: auth,
    });
    expect(missing.statusCode).toBe(404);
  });

  it("serves cached matrix cells through the bulk endpoint", async () => {
    const cellsUrl = url("/matrix-cells", { set: `run-${runId}`, direction: "short", horizonMinutes: 60 });
    const empty = await app.inject({ method: "GET", url: cellsUrl, headers: auth });
    expect(empty.statusCode).toBe(200);
    expect(empty.json<{ items: unknown[] }>().items).toHaveLength(0);

    await app.inject({
      method: "GET", url: url("/matrix-cell", { set: `run-${runId}`, direction: "short", tpPct: 4, slPct: 4, horizonMinutes: 60 }),
      headers: auth,
    });
    const filled = await app.inject({ method: "GET", url: cellsUrl, headers: auth });
    const filledBody = filled.json<{ items: { tpPct: number; slPct: number; riskExpectancy: number | null }[] }>();
    expect(filledBody.items).toHaveLength(1);
    expect(filledBody.items[0]).toMatchObject({ tpPct: 4, slPct: 4, riskExpectancy: 0 });
  });

  it("summarizes and clears only the analytics cache", async () => {
    await app.inject({
      method: "GET",
      url: url("/matrix-cell", {
        set: `run-${runId}`, direction: "short", tpPct: 4, slPct: 4, horizonMinutes: 60, setupMaturity: 0.5,
      }),
      headers: auth,
    });
    const summary = await app.inject({ method: "GET", url: `${base}/cache/summary`, headers: auth });
    const summaryBody = summary.json<{ tables: { name: string; count: number }[]; total: number }>();
    expect(summaryBody.total).toBeGreaterThan(0);

    const cleared = await app.inject({ method: "DELETE", url: `${base}/cache`, headers: auth });
    expect(cleared.statusCode).toBe(200);
    const clearedBody = cleared.json<{ deleted: { name: string; count: number }[]; total: number }>();
    expect(clearedBody.total).toBe(summaryBody.total);
    expect(clearedBody.deleted.some((entry) => entry.name === "analytics_matrix_cache" && entry.count > 0)).toBe(true);

    const afterClear = await app.inject({ method: "GET", url: `${base}/cache/summary`, headers: auth });
    expect(afterClear.json<{ total: number }>().total).toBe(0);

    // The replay run itself is untouched by the analytics cache clear.
    const runs = await db.selectFrom("replay_runs").selectAll().execute();
    expect(runs).toHaveLength(1);

    const recomputed = await app.inject({
      method: "GET",
      url: url("/matrix-cell", {
        set: `run-${runId}`, direction: "short", tpPct: 4, slPct: 4, horizonMinutes: 60, setupMaturity: 0.5,
      }),
      headers: auth,
    });
    expect(recomputed.json<MatrixCellBody>().cached).toBe(false);
  });
});
