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
import { collectOnce, drainBackfills, setCollectionTarget } from "../../src/ingest/continuous.js";
import { requestBackfill } from "../../src/ingest/backfill.js";
import { createGapRepairJobs, drainGapRepairs } from "../../src/ingest/gap-repairs.js";

const TEST_DATABASE_URL = process.env.TEST_DATABASE_URL;
const MINUTE = 60_000;
const T0 = Date.UTC(2024, 5, 1);

const minute = (offsetMinutes: number): Date => new Date(T0 + offsetMinutes * MINUTE);

function candle(openTime: Date): CandleInsert {
  return { openTime, open: "1", high: "1", low: "1", close: "1", volume: "1", quoteTurnover: "1" };
}

const NO_LIMITS = {
  maxPendingJobs: 100,
  maxWorkMinutes: 2_635_200,
  maxDatabaseBytes: Number.MAX_SAFE_INTEGER,
  estimatedBytesPerWorkMinute: 1,
};

function recordingTransport(hasBar: (openTimeMs: number) => boolean) {
  const requests: { start: number; end: number }[] = [];
  const transport: Transport = {
    fetch(url: string): Promise<TransportResponse> {
      const params = new URL(url).searchParams;
      const start = Number(params.get("start"));
      const end = Number(params.get("end"));
      requests.push({ start, end });
      const list: [string, string, string, string, string, string, string][] = [];
      for (let ms = end; ms >= start; ms -= MINUTE) {
        if (hasBar(ms)) list.push([String(ms), "2", "2", "2", "2", "1", "1"]);
      }
      return Promise.resolve({
        status: 200,
        headers: new Map(),
        bodyText: JSON.stringify({ retCode: 0, retMsg: "OK", result: { list } }),
      });
    },
  };
  return { transport, requests };
}

describe.skipIf(!TEST_DATABASE_URL)("gap verification and repair", () => {
  let db: Kysely<MarketDataDB>;
  let instrumentId: number;

  beforeAll(async () => {
    db = createDb(TEST_DATABASE_URL!);
    const migrationResult = await createMigrator(db).migrateToLatest();
    if (migrationResult.error) {
      throw new Error(`migration failed: ${JSON.stringify(migrationResult.error)}`);
    }
  });

  afterAll(async () => {
    await db.destroy();
  });

  beforeEach(async () => {
    await db.deleteFrom("gap_repair_jobs").execute();
    await db.deleteFrom("gap_verifications").execute();
    await db.deleteFrom("backfill_plan_items").execute();
    await db.deleteFrom("backfill_plans").execute();
    await db.deleteFrom("collection_targets").execute();
    await db.deleteFrom("backfill_jobs").execute();
    await db.deleteFrom("service_state").execute();
    await db.deleteFrom("candle_conflicts").execute();
    await db.deleteFrom("candles").execute();
    await db.deleteFrom("instruments").execute();
    await db.deleteFrom("exchanges").execute();
    await sql`DROP TABLE IF EXISTS candles_2024_05, candles_2024_06, candles_2024_07 CASCADE`.execute(db);
    instrumentId = await ensureInstrument(db, {
      exchangeCode: "bybit",
      category: "linear",
      symbol: "GAPUSDT",
    });
  });

  function makeClient(transport: Transport, clock: FakeClock): BybitClient {
    const gate = new AdaptiveGate(
      { ...DEFAULT_GATE_LIMITS, initialIntervalMs: 1, minIntervalMs: 1 },
      clock,
    );
    return new BybitClient(transport, gate, clock);
  }

  async function verificationRanges(): Promise<(readonly [number, number, string])[]> {
    const rows = await db
      .selectFrom("gap_verifications")
      .select(["from_ts", "to_ts", "source"])
      .orderBy("from_ts", "asc")
      .execute();
    return rows.map((row) => [row.from_ts.getTime(), row.to_ts.getTime(), row.source] as const);
  }

  it("marks minutes the exchange did not return as no_trade during downloads", async () => {
    await insertCandles(db, instrumentId, Array.from({ length: 5 }, (_, index) => candle(minute(index))));
    await setCollectionTarget(db, instrumentId, true);
    const clock = new FakeClock(T0 + 10 * MINUTE + 30_000);
    const { transport } = recordingTransport((ms) => minute(6).getTime() <= ms && ms <= minute(8).getTime());
    expect(await collectOnce(db, clock, NO_LIMITS)).toBe(1);
    expect(await drainBackfills({ db, client: makeClient(transport, clock) })).toBeGreaterThanOrEqual(1);
    const page = await readCandles(db, { instrumentId, from: minute(0), to: minute(10), limit: 5000 });
    expect(page.candles).toHaveLength(8);
    expect(await verificationRanges()).toEqual([
      [minute(5).getTime(), minute(5).getTime(), "download"],
      [minute(9).getTime(), minute(9).getTime(), "download"],
    ]);
  });

  it("does not re-request verified no-trade minutes on later jobs", async () => {
    await insertCandles(db, instrumentId, Array.from({ length: 5 }, (_, index) => candle(minute(index))));
    await db.insertInto("gap_verifications").values([
      { instrument_id: instrumentId, from_ts: minute(5), to_ts: minute(5), kind: "no_trade", source: "repair" },
      { instrument_id: instrumentId, from_ts: minute(9), to_ts: minute(9), kind: "no_trade", source: "repair" },
    ]).execute();
    const clock = new FakeClock(T0 + 10 * MINUTE + 30_000);
    const { transport, requests } = recordingTransport((ms) => ms !== minute(5).getTime() && ms !== minute(9).getTime());
    await requestBackfill(db, { instrumentId, fromTs: minute(0), toTs: minute(9), requestedBy: "test" }, NO_LIMITS);
    expect(await drainBackfills({ db, client: makeClient(transport, clock) })).toBeGreaterThanOrEqual(1);
    const jobs = await db.selectFrom("backfill_jobs").select("status").execute();
    expect(jobs[0]!.status).toBe("done");
    expect(requests).toEqual([{ start: minute(6).getTime(), end: minute(8).getTime() }]);
    const page = await readCandles(db, { instrumentId, from: minute(0), to: minute(10), limit: 5000 });
    expect(page.candles).toHaveLength(8);
  });

  it("repairs internal gaps: fills returned candles and marks the remainder no_trade", async () => {
    await insertCandles(db, instrumentId, [
      ...Array.from({ length: 5 }, (_, index) => candle(minute(index))),
      ...Array.from({ length: 5 }, (_, index) => candle(minute(15 + index))),
    ]);
    const clock = new FakeClock(T0 + 30 * MINUTE + 30_000);
    const { transport, requests } = recordingTransport((ms) => minute(7).getTime() <= ms && ms <= minute(8).getTime());
    const [jobId] = await createGapRepairJobs(db, [instrumentId]);
    expect(await drainGapRepairs({ db, client: makeClient(transport, clock) })).toBeGreaterThanOrEqual(1);
    const job = await db.selectFrom("gap_repair_jobs").selectAll().where("id", "=", jobId).executeTakeFirstOrThrow();
    expect(job.status).toBe("done");
    expect(job.filled_minutes).toBe(2);
    expect(job.verified_minutes).toBe(8);
    expect(job.gaps_processed).toBe(1);
    expect(requests[0]).toEqual({ start: minute(0).getTime(), end: minute(19).getTime() });
    expect(requests[1]).toEqual({ start: minute(5).getTime(), end: minute(14).getTime() });
    expect(await verificationRanges()).toEqual([
      [minute(5).getTime(), minute(6).getTime(), "repair"],
      [minute(9).getTime(), minute(14).getTime(), "repair"],
    ]);
    const page = await readCandles(db, { instrumentId, from: minute(0), to: minute(25), limit: 5000 });
    expect(page.candles).toHaveLength(12);
  });

  it("fails repair without marking when the exchange serves no data for the symbol", async () => {
    await insertCandles(db, instrumentId, [
      ...Array.from({ length: 5 }, (_, index) => candle(minute(index))),
      ...Array.from({ length: 5 }, (_, index) => candle(minute(15 + index))),
    ]);
    const { transport } = recordingTransport(() => false);
    const [jobId] = await createGapRepairJobs(db, [instrumentId]);
    await drainGapRepairs({ db, client: makeClient(transport, new FakeClock(T0 + 30 * MINUTE + 30_000)) });
    const job = await db.selectFrom("gap_repair_jobs").selectAll().where("id", "=", jobId).executeTakeFirstOrThrow();
    expect(job.status).toBe("failed");
    expect(job.error).toContain("no candles");
    expect(await verificationRanges()).toEqual([]);
    const page = await readCandles(db, { instrumentId, from: minute(0), to: minute(25), limit: 5000 });
    expect(page.candles).toHaveLength(10);
  });

  it("does not repair history before the first stored candle", async () => {
    await insertCandles(db, instrumentId, [
      ...Array.from({ length: 5 }, (_, index) => candle(minute(50 + index))),
      ...Array.from({ length: 5 }, (_, index) => candle(minute(60 + index))),
    ]);
    const clock = new FakeClock(T0 + 100 * MINUTE + 30_000);
    const { transport, requests } = recordingTransport(() => true);
    const [jobId] = await createGapRepairJobs(db, [instrumentId]);
    expect(await drainGapRepairs({ db, client: makeClient(transport, clock) })).toBeGreaterThanOrEqual(1);
    const job = await db.selectFrom("gap_repair_jobs").selectAll().where("id", "=", jobId).executeTakeFirstOrThrow();
    expect(job.status).toBe("done");
    expect(job.filled_minutes).toBe(5);
    expect(job.verified_minutes).toBe(0);
    for (const request of requests) {
      expect(request.start).toBeGreaterThanOrEqual(minute(50).getTime());
    }
    expect(await verificationRanges()).toEqual([]);
    const page = await readCandles(db, { instrumentId, from: minute(0), to: minute(80), limit: 5000 });
    expect(page.candles).toHaveLength(15);
    expect(page.candles[0]!.openTimeUnix).toBe(Math.floor(minute(50).getTime() / 1000));
  });

  it("excludes verified minutes from admission work accounting", async () => {
    await insertCandles(db, instrumentId, Array.from({ length: 5 }, (_, index) => candle(minute(index))));
    await db.insertInto("gap_verifications").values({
      instrument_id: instrumentId,
      from_ts: minute(5),
      to_ts: minute(9),
      kind: "no_trade",
      source: "repair",
    }).execute();
    const jobId = await requestBackfill(
      db,
      { instrumentId, fromTs: minute(0), toTs: minute(9), requestedBy: "test" },
      { maxPendingJobs: 100, maxWorkMinutes: 1, maxDatabaseBytes: Number.MAX_SAFE_INTEGER, estimatedBytesPerWorkMinute: 1 },
    );
    expect(jobId).toBeGreaterThan(0);
  });
});
