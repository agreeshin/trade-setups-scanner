/* eslint-disable @typescript-eslint/no-unsafe-assignment, @typescript-eslint/no-unsafe-call, @typescript-eslint/no-unsafe-member-access --
   kysely/migration types resolve in tsc but not in the ESLint project service. */
import { afterAll, beforeAll, beforeEach, describe, expect, it } from "vitest";
import { sql, type Kysely } from "kysely";
import { createDb } from "../../src/db/db.js";
import { createMigrator } from "../../src/db/migrator.js";
import type { MarketDataDB } from "../../src/db/schema.js";
import { ensureInstrument } from "../../src/storage/instruments.js";
import { readCandles } from "../../src/storage/candles.js";
import { insertCandles } from "../../src/storage/candles.js";
import { BybitClient } from "../../src/ingest/bybit.js";
import { AdaptiveGate, DEFAULT_GATE_LIMITS } from "../../src/ingest/gate.js";
import { FakeClock } from "../../src/ingest/clock.js";
import type { Transport, TransportResponse } from "../../src/ingest/transport.js";
import {
  cancelBackfillRequest,
  requestBackfill,
  runBackfillJobOnce,
} from "../../src/ingest/backfill.js";

const TEST_DATABASE_URL = process.env.TEST_DATABASE_URL;
const MINUTE = 60_000;
const DAY = 1440 * MINUTE;
const T0 = Date.UTC(2024, 5, 1);

const minute = (offsetMinutes: number): Date => new Date(T0 + offsetMinutes * MINUTE);
const unix = (date: Date): number => Math.floor(date.getTime() / 1000);

class RecordingTransport {
  readonly calls: string[] = [];

  constructor(private readonly handler: (url: string) => TransportResponse) {}

  fetch(url: string): Promise<TransportResponse> {
    this.calls.push(url);
    return Promise.resolve(this.handler(url));
  }
}

function fullHistoryTransport(): RecordingTransport {
  return new RecordingTransport((url) => {
    const params = new URL(url).searchParams;
    const start = Number(params.get("start"));
    const end = Number(params.get("end"));
    const list: [string, string, string, string, string, string, string][] = [];
    for (let ms = end; ms >= start; ms -= MINUTE) {
      const close = String(Math.floor(ms / MINUTE));
      list.push([String(ms), close, close, close, close, "1", "2"]);
    }
    return {
      status: 200,
      headers: new Map(),
      bodyText: JSON.stringify({ retCode: 0, retMsg: "OK", result: { list } }),
    };
  });
}

function makeClient(transport: Transport, clock: FakeClock): BybitClient {
  const gate = new AdaptiveGate(
    { ...DEFAULT_GATE_LIMITS, initialIntervalMs: 1, minIntervalMs: 1 },
    clock,
  );
  return new BybitClient(transport, gate, clock);
}

describe.skipIf(!TEST_DATABASE_URL)("backfill jobs", () => {
  let db: Kysely<MarketDataDB>;
  let instrumentId: number;

  beforeAll(async () => {
    db = createDb(TEST_DATABASE_URL!);
    const migrator = createMigrator(db);
    const migrationResult = await migrator.migrateToLatest();
    if (migrationResult.error) {
      throw migrationResult.error;
    }
  });

  beforeEach(async () => {
    await db.deleteFrom("collection_targets").execute();
    await db.deleteFrom("backfill_jobs").execute();
    await db.deleteFrom("candle_conflicts").execute();
    await db.deleteFrom("candles").execute();
    await db.deleteFrom("instruments").execute();
    await db.deleteFrom("exchanges").execute();
    await db.deleteFrom("service_state").execute();
    await sql`DROP TABLE IF EXISTS candles_2024_05, candles_2024_06, candles_2024_07 CASCADE`.execute(
      db,
    );
    instrumentId = await ensureInstrument(db, {
      exchangeCode: "bybit",
      category: "linear",
      symbol: "BFILLUSDT",
    });
  });

  afterAll(async () => {
    await db.destroy();
  });

  it("creates a job and reuses an active covering job", async () => {
    const firstId = await requestBackfill(db, {
      instrumentId,
      fromTs: minute(0),
      toTs: minute(2880),
      requestedBy: "client-a",
    });
    const coveringId = await requestBackfill(db, {
      instrumentId,
      fromTs: minute(60),
      toTs: minute(2820),
      requestedBy: "client-b",
    });
    expect(coveringId).toBe(firstId);
    const outsideId = await requestBackfill(db, {
      instrumentId,
      fromTs: minute(2880),
      toTs: minute(4320),
      requestedBy: "client-a",
    });
    expect(outsideId).not.toBe(firstId);
    const jobs = await db.selectFrom("backfill_jobs").selectAll().execute();
    const reused = jobs.find((job) => job.id === firstId);
    expect(reused?.requested_by).toEqual(["client-a", "client-b"]);
  });

  it("fills candles across chunks with checkpoints and completes", async () => {
    const clock = new FakeClock(T0 + 3 * DAY + MINUTE);
    const transport = fullHistoryTransport();
    const client = makeClient(transport, clock);
    const jobId = await requestBackfill(db, {
      instrumentId,
      fromTs: minute(0),
      toTs: minute(2879),
      requestedBy: "client-a",
    });
    const processed = await runBackfillJobOnce({ db, client, chunkMinutes: 1440 });
    expect(processed).toBe(true);
    const job = await db
      .selectFrom("backfill_jobs")
      .where("id", "=", jobId)
      .selectAll()
      .executeTakeFirstOrThrow();
    expect(job.status).toBe("done");
    expect(job.checkpoint_ts?.getTime()).toBe(minute(2879).getTime());
    const page = await readCandles(db, {
      instrumentId,
      from: minute(0),
      to: minute(2880),
      limit: 5000,
    });
    expect(page.candles).toHaveLength(2880);
    expect(page.candles[0]?.openTimeUnix).toBe(unix(minute(0)));
    expect(page.candles.at(-1)?.close).toBe(String(Math.floor(minute(2879).getTime() / MINUTE)));
  });

  it("resumes from the stored checkpoint without refetching earlier chunks", async () => {
    const clock = new FakeClock(T0 + 3 * DAY + MINUTE);
    const transport = fullHistoryTransport();
    const client = makeClient(transport, clock);
    const manual = await db
      .insertInto("backfill_jobs")
      .values({
        instrument_id: instrumentId,
        from_ts: minute(0),
        to_ts: minute(2879),
        status: "queued",
        checkpoint_ts: minute(1439),
        requested_by: ["client-a"],
      })
      .returning(["id"])
      .executeTakeFirstOrThrow();
    expect(manual.id).toBeGreaterThan(0);
    const processed = await runBackfillJobOnce({ db, client, chunkMinutes: 1440 });
    expect(processed).toBe(true);
    const firstFetchStart = Number(new URL(transport.calls[0] ?? "").searchParams.get("start"));
    expect(firstFetchStart).toBe(minute(1440).getTime());
    expect(transport.calls.length).toBeLessThanOrEqual(2);
  });

  it("keeps a job alive when only one of two clients cancels", async () => {
    const clock = new FakeClock(T0 + 2 * DAY + MINUTE);
    const client = makeClient(fullHistoryTransport(), clock);
    const jobId = await requestBackfill(db, {
      instrumentId,
      fromTs: minute(0),
      toTs: minute(1439),
      requestedBy: "client-a",
    });
    await requestBackfill(db, {
      instrumentId,
      fromTs: minute(0),
      toTs: minute(1439),
      requestedBy: "client-b",
    });
    await cancelBackfillRequest(db, jobId, "client-a");
    const processed = await runBackfillJobOnce({ db, client, chunkMinutes: 1440 });
    expect(processed).toBe(true);
    const job = await db
      .selectFrom("backfill_jobs")
      .where("id", "=", jobId)
      .selectAll()
      .executeTakeFirstOrThrow();
    expect(job.status).toBe("done");
    expect(job.requested_by).toEqual(["client-b"]);
  });

  it("cancels a job when its last client cancels before completion", async () => {
    const clock = new FakeClock(T0 + 2 * DAY + MINUTE);
    const transport = fullHistoryTransport();
    const client = makeClient(transport, clock);
    const jobId = await requestBackfill(db, {
      instrumentId,
      fromTs: minute(0),
      toTs: minute(1439),
      requestedBy: "client-a",
    });
    await cancelBackfillRequest(db, jobId, "client-a");
    const processed = await runBackfillJobOnce({ db, client, chunkMinutes: 1440 });
    expect(processed).toBe(true);
    const job = await db
      .selectFrom("backfill_jobs")
      .where("id", "=", jobId)
      .selectAll()
      .executeTakeFirstOrThrow();
    expect(job.status).toBe("cancelled");
    expect(transport.calls).toHaveLength(0);
  });

  it("overlapping jobs store each minute once without conflicts", async () => {
    const clock = new FakeClock(T0 + 5 * DAY + MINUTE);
    const client = makeClient(fullHistoryTransport(), clock);
    await requestBackfill(db, {
      instrumentId,
      fromTs: minute(0),
      toTs: minute(2879),
      requestedBy: "client-a",
    });
    await requestBackfill(db, {
      instrumentId,
      fromTs: minute(1440),
      toTs: minute(4319),
      requestedBy: "client-b",
    });
    await runBackfillJobOnce({ db, client, chunkMinutes: 1440 });
    await runBackfillJobOnce({ db, client, chunkMinutes: 1440 });
    const jobs = await db.selectFrom("backfill_jobs").selectAll().execute();
    expect(jobs.map((job) => job.status).sort()).toEqual(["done", "done"]);
    const conflicts = await db.selectFrom("candle_conflicts").selectAll().execute();
    expect(conflicts).toHaveLength(0);
    const page = await readCandles(db, {
      instrumentId,
      from: minute(0),
      to: minute(4320),
      limit: 5000,
    });
    expect(page.candles).toHaveLength(4320);
  });

  it("does not fetch already covered minutes", async () => {
    const clock = new FakeClock(T0 + 3 * DAY + MINUTE);
    const transport = fullHistoryTransport();
    const client = makeClient(transport, clock);
    await insertCandles(
      db,
      instrumentId,
      Array.from({ length: 1440 }, (_, index) => ({
        openTime: minute(index),
        open: "1",
        high: "1",
        low: "1",
        close: "1",
        volume: "1",
        quoteTurnover: "1",
      })),
    );
    await requestBackfill(db, {
      instrumentId,
      fromTs: minute(0),
      toTs: minute(2879),
      requestedBy: "client-a",
    });
    const processed = await runBackfillJobOnce({ db, client, chunkMinutes: 1440 });
    expect(processed).toBe(true);
    expect(transport.calls.length).toBeGreaterThan(0);
    for (const call of transport.calls) {
      const start = Number(new URL(call).searchParams.get("start"));
      expect(start).toBeGreaterThanOrEqual(minute(1440).getTime());
    }
    const page = await readCandles(db, {
      instrumentId,
      from: minute(0),
      to: minute(2880),
      limit: 5000,
    });
    expect(page.candles).toHaveLength(2880);
  });

  it("fetches nothing for a fully covered range", async () => {
    const clock = new FakeClock(T0 + 2 * DAY + MINUTE);
    const transport = fullHistoryTransport();
    const client = makeClient(transport, clock);
    await insertCandles(
      db,
      instrumentId,
      Array.from({ length: 1440 }, (_, index) => ({
        openTime: minute(index),
        open: "1",
        high: "1",
        low: "1",
        close: "1",
        volume: "1",
        quoteTurnover: "1",
      })),
    );
    await requestBackfill(db, {
      instrumentId,
      fromTs: minute(0),
      toTs: minute(1439),
      requestedBy: "client-a",
    });
    const processed = await runBackfillJobOnce({ db, client, chunkMinutes: 1440 });
    expect(processed).toBe(true);
    expect(transport.calls).toHaveLength(0);
    const job = await db.selectFrom("backfill_jobs").selectAll().executeTakeFirstOrThrow();
    expect(job.status).toBe("done");
  });
});
