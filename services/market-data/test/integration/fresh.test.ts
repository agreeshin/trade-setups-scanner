import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it } from "vitest";
import type { Kysely } from "kysely";
import { createDb } from "../../src/db/db.js";
import { createMigrator } from "../../src/db/migrator.js";
import type { MarketDataDB } from "../../src/db/schema.js";
import { ensureInstrument } from "../../src/storage/instruments.js";
import { insertCandles } from "../../src/storage/candles.js";
import { collectOnce, setCollectionTarget } from "../../src/ingest/continuous.js";
import { BybitClient, type KlineBar } from "../../src/ingest/bybit.js";
import { AdaptiveGate, DEFAULT_GATE_LIMITS } from "../../src/ingest/gate.js";
import { FakeClock } from "../../src/ingest/clock.js";
import { FreshCollector, DEFAULT_FRESH_OPTIONS } from "../../src/ingest/fresh.js";
import { runBackfillJobOnce } from "../../src/ingest/backfill.js";
import type { Transport } from "../../src/ingest/transport.js";

const TEST_DATABASE_URL = process.env.TEST_DATABASE_URL;
const T0 = Date.UTC(2024, 5, 1);
const MINUTE = 60_000;
const minute = (n: number) => new Date(T0 + n * MINUTE);
const candle = (n: number, close = "1.0000000000000000001"): KlineBar => ({ openTime: minute(n), open: close,
  high: close, low: close, close, volume: "1", quoteTurnover: "2" });

function deferred() {
  let release = () => undefined as void;
  return { promise: new Promise<void>((resolve) => { release = resolve; }), resolve: () => release() };
}

describe.skipIf(!TEST_DATABASE_URL)("independent fresh collector", () => {
  let db: Kysely<MarketDataDB>;
  let ids: number[];
  let collector: FreshCollector | undefined;
  let client: BybitClient | undefined;
  let clock: FakeClock;
  beforeAll(async () => {
    db = createDb(TEST_DATABASE_URL!);
    const migration = await createMigrator(db).migrateToLatest();
    if (migration.error) throw new Error("test migration failed", { cause: migration.error });
  });
  afterAll(async () => { await db.destroy(); });
  beforeEach(() => {
    ids = [];
    clock = new FakeClock(T0 + 10 * MINUTE + 5000);
  });
  afterEach(async () => {
    await collector?.stop(); collector = undefined;
    client?.close(); client = undefined;
    if (ids.length) {
      for (const table of ["collection_targets", "backfill_jobs", "candle_conflicts", "candles", "gap_verifications"] as const)
        await db.deleteFrom(table).where("instrument_id", "in", ids).execute();
      await db.deleteFrom("instruments").where("id", "in", ids).execute();
    }
  });
  async function target(symbol = "FRESHUSDT") {
    const id = await ensureInstrument(db, { exchangeCode: "bybit", category: "linear", symbol });
    ids.push(id); await setCollectionTarget(db, id, true); return id;
  }
  function make(transport: Transport, maxBufferedItems = 4096, gate?: AdaptiveGate) {
    client = new BybitClient(transport, gate ?? new AdaptiveGate({ ...DEFAULT_GATE_LIMITS, initialIntervalMs: 1, minIntervalMs: 1 }, clock), clock);
    collector = new FreshCollector(db, client, clock, { ...DEFAULT_FRESH_OPTIONS, repairMinutes: 2, maxBufferedItems },
      () => undefined, { setSymbols: () => undefined, stop: () => undefined });
    return collector;
  }
  const empty: Transport = { fetch: () => Promise.resolve({ status: 200, headers: new Map(), bodyText: JSON.stringify({ retCode: 0, result: { list: [] } }) }) };

  it("stores latest closed minutes while the historical job queue is saturated", async () => {
    const id = await target();
    await insertCandles(db, id, [candle(0)]);
    await db.insertInto("backfill_jobs").values({ instrument_id: id, from_ts: minute(1), to_ts: minute(8), status: "queued", requested_by: ["history"] }).execute();
    const fresh = make({ fetch: (url) => {
      const query = new URL(url).searchParams;
      const list = [];
      for (let at = Number(query.get("end")); at >= Number(query.get("start")); at -= MINUTE)
        list.push([String(at), "2", "2", "2", "2", "1", "2"]);
      return Promise.resolve({ status: 200, headers: new Map(), bodyText: JSON.stringify({ retCode: 0, result: { list } }) });
    } });
    await fresh.pollOnce(); await fresh.idle();
    const state = await db.selectFrom("instrument_watermarks").where("instrument_id", "=", id).selectAll().executeTakeFirstOrThrow();
    expect(state.latest_open_time).toEqual(minute(9));
    expect(await db.selectFrom("backfill_jobs").where("instrument_id", "=", id).selectAll().execute()).toHaveLength(1);
    expect(await db.selectFrom("gap_verifications").where("instrument_id", "=", id).selectAll().execute()).toHaveLength(0);
    const tail = await db.selectFrom("collection_targets").where("instrument_id", "=", id).select("tail_next_ts").executeTakeFirstOrThrow();
    expect(tail.tail_next_ts).toEqual(minute(1));
  });
  it("keeps empty fresh responses pending and writes WS bars independently of blocked REST", async () => {
    const id = await target();
    const gate = new AdaptiveGate({ ...DEFAULT_GATE_LIMITS, maxConcurrent: 1 }, clock);
    const hold = deferred();
    const admitted = deferred();
    const blocked = gate.run(() => { admitted.resolve(); return hold.promise; });
    await admitted.promise;
    const fresh = make(empty, 4096, gate);
    await fresh.pollOnce();
    expect(fresh.enqueue("FRESHUSDT", candle(9))).toBe(true);
    await expect.poll(async () => (await db.selectFrom("candles").where("instrument_id", "=", id).selectAll().execute()).length).toBe(1);
    // Stop must cancel the queued REST permit, not wait for a ban/request slot.
    await fresh.stop();
    hold.resolve(); await blocked;
    expect(await db.selectFrom("gap_verifications").where("instrument_id", "=", id).selectAll().execute()).toHaveLength(0);
  });
  it("does not convert an empty fresh REST response into no-trade or latest availability", async () => {
    const id = await target();
    const fresh = make(empty);
    await fresh.pollOnce(); await fresh.idle();
    const state = await db.selectFrom("instrument_watermarks").where("instrument_id", "=", id).selectAll().executeTakeFirstOrThrow();
    expect(state.latest_open_time).toBeNull();
    expect(await db.selectFrom("gap_verifications").where("instrument_id", "=", id).selectAll().execute()).toHaveLength(0);
  });
  it("preserves an old tail repair anchor after newer WS data and collector restart", async () => {
    const id = await target(); await insertCandles(db, id, [candle(0)]);
    const fresh = make(empty); await fresh.pollOnce(); await fresh.idle();
    fresh.enqueue("FRESHUSDT", candle(9)); await fresh.idle(); await fresh.stop();
    await collectOnce(db, clock);
    const job = await db.selectFrom("backfill_jobs").where("instrument_id", "=", id).selectAll().executeTakeFirstOrThrow();
    expect(job.from_ts).toEqual(minute(1)); expect(job.to_ts).toEqual(minute(9));
  });
  it("bounds buffered items/bytes and frees buffers after stop", async () => {
    const id = await target();
    const fresh = make(empty, 1); await fresh.pollOnce(); await fresh.idle();
    expect(fresh.enqueue("FRESHUSDT", candle(9))).toBe(true);
    expect(fresh.enqueue("FRESHUSDT", candle(8))).toBe(false);
    await fresh.idle();
    expect(fresh.enqueue("FRESHUSDT", { ...candle(8), close: "1".repeat(5 * 1024 * 1024) })).toBe(false);
    await fresh.stop();
    fresh.enqueue("FRESHUSDT", candle(8)); await fresh.idle();
    expect(await db.selectFrom("candles").where("instrument_id", "=", id).selectAll().execute()).toHaveLength(1);
  });
  it("records differing concurrent WS/REST inserts without overwriting or inflated insertion counts", async () => {
    const id = await target();
    const results = await Promise.all([insertCandles(db, id, [candle(9)]), insertCandles(db, id, [candle(9, "2.0000000000000000001")])]);
    expect(results.reduce((sum, r) => sum + r.inserted, 0)).toBe(1);
    expect(await db.selectFrom("candles").where("instrument_id", "=", id).selectAll().execute()).toHaveLength(1);
    expect(await db.selectFrom("candle_conflicts").where("instrument_id", "=", id).selectAll().execute()).toHaveLength(1);
  });
  it("rechecks a publication-lag tail after restart at the final checkpoint", async () => {
    const id = await target(); await insertCandles(db, id, Array.from({ length: 9 }, (_, n) => candle(n)));
    await db.updateTable("collection_targets").set({ tail_next_ts: minute(1), tail_initialized: true }).where("instrument_id", "=", id).execute();
    await db.insertInto("backfill_jobs").values({ instrument_id: id, from_ts: minute(1), to_ts: minute(9), checkpoint_ts: minute(9),
      status: "queued", requested_by: ["continuous"] }).execute();
    make(empty);
    await runBackfillJobOnce({ db, client: client! });
    const state = await db.selectFrom("collection_targets").where("instrument_id", "=", id).select("tail_next_ts").executeTakeFirstOrThrow();
    expect(state.tail_next_ts).toEqual(minute(9));
  });
  it("advances an already stored short tail without historical admission or queued work", async () => {
    const id = await target(); await insertCandles(db, id, Array.from({ length: 10 }, (_, n) => candle(n)));
    await db.updateTable("collection_targets").set({ tail_next_ts: minute(8), tail_initialized: true }).where("instrument_id", "=", id).execute();
    await collectOnce(db, clock, { maxPendingJobs: 0, maxWorkMinutes: 0, maxDatabaseBytes: 0, estimatedBytesPerWorkMinute: 1 });
    expect(await db.selectFrom("backfill_jobs").where("instrument_id", "=", id).selectAll().execute()).toHaveLength(0);
    const state = await db.selectFrom("collection_targets").where("instrument_id", "=", id).select("tail_next_ts").executeTakeFirstOrThrow();
    expect(state.tail_next_ts).toEqual(minute(10));
  });
  it("bootstraps existing latest availability before inserting older historical candles", async () => {
    const id = await target(); await insertCandles(db, id, [candle(9)]);
    await db.deleteFrom("instrument_watermarks").where("instrument_id", "=", id).execute();
    await insertCandles(db, id, [candle(0)]);
    const state = await db.selectFrom("instrument_watermarks").where("instrument_id", "=", id).selectAll().executeTakeFirstOrThrow();
    expect(state.latest_open_time).toEqual(minute(9));
  });
});
