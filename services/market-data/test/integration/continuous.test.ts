 
import { afterAll, beforeAll, beforeEach, describe, expect, it } from "vitest";
import { sql, type Kysely } from "kysely";
import { createDb } from "../../src/db/db.js";
import { createMigrator } from "../../src/db/migrator.js";
import type { MarketDataDB } from "../../src/db/schema.js";
import { ensureInstrument } from "../../src/storage/instruments.js";
import { insertCandles, readCandles, type CandleInsert } from "../../src/storage/candles.js";
import { BybitClient } from "../../src/ingest/bybit.js";
import { AdaptiveGate, DEFAULT_GATE_LIMITS } from "../../src/ingest/gate.js";
import { FakeClock } from "../../src/ingest/clock.js";
import type { Transport, TransportResponse } from "../../src/ingest/transport.js";
import {
  collectOnce,
  drainBackfills,
  setCollectionTarget,
} from "../../src/ingest/continuous.js";

const TEST_DATABASE_URL = process.env.TEST_DATABASE_URL;
const MINUTE = 60_000;
const T0 = Date.UTC(2024, 5, 1);

const minute = (offsetMinutes: number): Date => new Date(T0 + offsetMinutes * MINUTE);

function candle(openTime: Date): CandleInsert {
  return {
    openTime,
    open: "1",
    high: "1",
    low: "1",
    close: "1",
    volume: "1",
    quoteTurnover: "1",
  };
}

const fullHistoryTransport: Transport = {
  fetch(url: string): Promise<TransportResponse> {
    const params = new URL(url).searchParams;
    const start = Number(params.get("start"));
    const end = Number(params.get("end"));
    const list: [string, string, string, string, string, string, string][] = [];
    for (let ms = end; ms >= start; ms -= MINUTE) {
      list.push([String(ms), "2", "2", "2", "2", "1", "1"]);
    }
    return Promise.resolve({
      status: 200,
      headers: new Map(),
      bodyText: JSON.stringify({ retCode: 0, retMsg: "OK", result: { list } }),
    });
  },
};

describe.skipIf(!TEST_DATABASE_URL)("continuous collection", () => {
  let db: Kysely<MarketDataDB>;
  let instrumentId: number;
  let noHistoryInstrumentId: number;

  beforeAll(async () => {
    db = createDb(TEST_DATABASE_URL!);
    const migrator = createMigrator(db);
    const migrationResult = await migrator.migrateToLatest();
    if (migrationResult.error) {
      throw new Error(`migration failed: ${JSON.stringify(migrationResult.error)}`);
    }
  });

  afterAll(async () => {
    await db.destroy();
  });

  beforeEach(async () => {
    await db.deleteFrom("collection_targets").execute();
    await db.deleteFrom("backfill_jobs").execute();
    await db.deleteFrom("service_state").execute();
    await db.deleteFrom("candle_conflicts").execute();
    await db.deleteFrom("candles").execute();
    await db.deleteFrom("instruments").execute();
    await db.deleteFrom("exchanges").execute();
    await sql`DROP TABLE IF EXISTS candles_2024_05, candles_2024_06, candles_2024_07 CASCADE`.execute(
      db,
    );
    instrumentId = await ensureInstrument(db, {
      exchangeCode: "bybit",
      category: "linear",
      symbol: "CONTUSDT",
    });
    noHistoryInstrumentId = await ensureInstrument(db, {
      exchangeCode: "bybit",
      category: "linear",
      symbol: "EMPTYUSDT",
    });
  });

  function makeClient(clock: FakeClock): BybitClient {
    const gate = new AdaptiveGate(
      { ...DEFAULT_GATE_LIMITS, initialIntervalMs: 1, minIntervalMs: 1 },
      clock,
    );
    return new BybitClient(fullHistoryTransport, gate, clock);
  }

  it("catches up the tail up to the last closed minute", async () => {
    await insertCandles(
      db,
      instrumentId,
      Array.from({ length: 10 }, (_, index) => candle(minute(index))),
    );
    await setCollectionTarget(db, instrumentId, true);

    const clock = new FakeClock(T0 + 15 * MINUTE + 30_000);
    const client = makeClient(clock);
    const scheduled = await collectOnce(db, clock);
    expect(scheduled).toBe(1);
    const processed = await drainBackfills({ db, client });
    expect(processed).toBeGreaterThanOrEqual(1);

    const page = await readCandles(db, {
      instrumentId,
      from: minute(0),
      to: minute(20),
      limit: 5000,
    });
    expect(page.candles).toHaveLength(15);
    expect(page.candles.at(-1)?.openTimeUnix).toBe(
      Math.floor(minute(14).getTime() / 1000),
    );
  });

  it("covers minutes that closed while the previous catch-up was still running", async () => {
    await insertCandles(
      db,
      instrumentId,
      Array.from({ length: 10 }, (_, index) => candle(minute(index))),
    );
    await setCollectionTarget(db, instrumentId, true);

    const clock15 = new FakeClock(T0 + 15 * MINUTE + 30_000);
    expect(await collectOnce(db, clock15)).toBe(1);
    await drainBackfills({ db, client: makeClient(clock15) });
    const first = await readCandles(db, {
      instrumentId,
      from: minute(0),
      to: minute(20),
      limit: 5000,
    });
    expect(first.candles).toHaveLength(15);

    const clock75 = new FakeClock(T0 + 75 * MINUTE + 30_000);
    expect(await collectOnce(db, clock75)).toBe(1);
    await drainBackfills({ db, client: makeClient(clock75) });

    const page = await readCandles(db, {
      instrumentId,
      from: minute(0),
      to: minute(80),
      limit: 5000,
    });
    expect(page.candles).toHaveLength(75);
    expect(page.candles.at(-1)?.openTimeUnix).toBe(
      Math.floor(minute(74).getTime() / 1000),
    );
    const gaps = page.candles.flatMap((row, index) => {
      if (index === 0) return [];
      return row.openTimeUnix === page.candles[index - 1]!.openTimeUnix + 60
        ? []
        : [row.openTimeUnix];
    });
    expect(gaps).toEqual([]);
  });

  it("skips targets without stored history", async () => {
    await setCollectionTarget(db, noHistoryInstrumentId, true);
    const clock = new FakeClock(T0 + 100 * MINUTE);
    const scheduled = await collectOnce(db, clock);
    expect(scheduled).toBe(0);
    const jobs = await db.selectFrom("backfill_jobs").selectAll().execute();
    expect(jobs).toHaveLength(0);
  });

  it("keeps accepted work drainable when a later target reaches the queue limit", async () => {
    await insertCandles(db, instrumentId, [candle(minute(0))]);
    await insertCandles(db, noHistoryInstrumentId, [candle(minute(0))]);
    await setCollectionTarget(db, instrumentId, true);
    await setCollectionTarget(db, noHistoryInstrumentId, true);
    const clock = new FakeClock(T0 + 5 * MINUTE + 1);

    const scheduled = await collectOnce(db, clock, {
      maxPendingJobs: 1,
      maxWorkMinutes: 10,
      maxDatabaseBytes: Number.MAX_SAFE_INTEGER,
      estimatedBytesPerWorkMinute: 1,
    });

    expect(scheduled).toBe(1);
    expect(await db.selectFrom("backfill_jobs").selectAll().execute()).toHaveLength(1);
    expect(await drainBackfills({ db, client: makeClient(clock) })).toBe(1);
  });

  it("schedules nothing when already up to date", async () => {
    await insertCandles(
      db,
      instrumentId,
      Array.from({ length: 10 }, (_, index) => candle(minute(index))),
    );
    await setCollectionTarget(db, instrumentId, true);
    const clock = new FakeClock(T0 + 10 * MINUTE + 1);
    const scheduled = await collectOnce(db, clock);
    expect(scheduled).toBe(0);
    const jobs = await db.selectFrom("backfill_jobs").selectAll().execute();
    expect(jobs).toHaveLength(0);
  });
});
