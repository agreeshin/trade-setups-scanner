import { afterAll, beforeAll, beforeEach, describe, expect, it } from "vitest";
 
import { sql, type Kysely } from "kysely";
import { createDb } from "../../src/db/db.js";
import { createMigrator } from "../../src/db/migrator.js";
import type { MarketDataDB } from "../../src/db/schema.js";
import { ensureInstrument } from "../../src/storage/instruments.js";
import { insertCandleBatch, insertCandles, readCandles, type CandleInsert } from "../../src/storage/candles.js";
import { readGuards } from "../../src/storage/read-guards.js";
import { getCoverage } from "../../src/storage/coverage.js";
import { ensureMonthPartitions } from "../../src/storage/partitions.js";

const TEST_DATABASE_URL = process.env.TEST_DATABASE_URL;

const minute = (offsetMinutes: number): Date =>
  new Date(Date.UTC(2024, 5, 30, 23, 57) + offsetMinutes * 60_000);

function candle(openTime: Date, close: string): CandleInsert {
  return {
    openTime,
    open: close,
    high: close,
    low: close,
    close,
    volume: "1.5",
    quoteTurnover: "1500.25",
  };
}

describe.skipIf(!TEST_DATABASE_URL)("market data storage", () => {
  let db: Kysely<MarketDataDB>;
  let instrumentId: number;

  beforeAll(async () => {
    db = createDb(TEST_DATABASE_URL!);
    const migrator = createMigrator(db);
    const result = await migrator.migrateToLatest();
    if (result.error) {
      throw new Error(`migration failed: ${JSON.stringify(result.error)}`);
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
    await sql`DROP TABLE IF EXISTS candles_2024_06, candles_2024_07, candles_2024_08 CASCADE`.execute(
      db,
    );
    instrumentId = await ensureInstrument(db, {
      exchangeCode: "bybit",
      category: "linear",
      symbol: "TESTUSDT",
    });
  });

  it("ensureInstrument is idempotent and returns the same id", async () => {
    const second = await ensureInstrument(db, {
      exchangeCode: "bybit",
      category: "linear",
      symbol: "TESTUSDT",
    });
    expect(second).toBe(instrumentId);
  });

  it("creates month partitions on demand, including historical months", async () => {
    const created = await ensureMonthPartitions(db, minute(0), minute(3600));
    expect(created).toContain("candles_2024_06");
    expect(created).toContain("candles_2024_07");
    const partitions = await sql<{ relname: string }>`
      SELECT c.relname FROM pg_class c
      JOIN pg_inherits i ON i.inhrelid = c.oid
      WHERE i.inhparent = 'candles'::regclass
      ORDER BY c.relname`.execute(db);
    expect(partitions.rows.map((row) => row.relname)).toContain("candles_2024_06");
  });

  it("inserts candles across partition boundaries", async () => {
    const result = await insertCandles(db, instrumentId, [
      candle(minute(0), "100"),
      candle(minute(1), "101"),
      candle(minute(3), "102"), // crosses into 2024-07
    ]);
    expect(result).toEqual({ inserted: 3, conflicting: 0 });
  });

  it("re-inserting identical candles inserts nothing and reports no conflicts", async () => {
    const candles = [candle(minute(0), "100"), candle(minute(1), "101")];
    await insertCandles(db, instrumentId, candles);
    const second = await insertCandles(db, instrumentId, candles);
    expect(second).toEqual({ inserted: 0, conflicting: 0 });
    const stored = await readCandles(db, { instrumentId, from: minute(0), to: minute(5) });
    expect(stored.candles).toHaveLength(2);
  });

  it("keeps stored values and records a conflict when a duplicate differs", async () => {
    await insertCandles(db, instrumentId, [candle(minute(0), "100")]);
    const result = await insertCandles(db, instrumentId, [candle(minute(0), "999")]);
    expect(result).toEqual({ inserted: 0, conflicting: 1 });
    const stored = await readCandles(db, { instrumentId, from: minute(0), to: minute(1) });
    expect(stored.candles[0]?.close).toBe("100");
    const conflicts = await db.selectFrom("candle_conflicts").selectAll().execute();
    expect(conflicts).toHaveLength(1);
    // Conflicting duplicate is reported once, not duplicated.
    await insertCandles(db, instrumentId, [candle(minute(0), "999")]);
    const conflictsAfterRepeat = await db
      .selectFrom("candle_conflicts")
      .selectAll()
      .execute();
    expect(conflictsAfterRepeat).toHaveLength(1);
  });

  it("treats numerically equal decimals as identical", async () => {
    await insertCandles(db, instrumentId, [candle(minute(0), "100")]);
    const same = await insertCandles(db, instrumentId, [candle(minute(0), "100.00")]);
    expect(same).toEqual({ inserted: 0, conflicting: 0 });
  });

  it("preserves first canonical and differing OHLCV/turnover observations inside one batch", async () => {
    const first = candle(minute(0), "100");
    const result = await insertCandles(db, instrumentId, [first,
      { ...first, close: "100.00", volume: "1.500" },
      { ...first, close: "999", quoteTurnover: "999.001" },
    ]);
    expect(result).toEqual({ inserted: 1, conflicting: 1 });
    const stored = await readCandles(db, { instrumentId, from: minute(0), to: minute(1) });
    expect(stored.candles[0]).toMatchObject({ close: "100", volume: "1.5", quoteTurnover: "1500.25" });
    expect(await db.selectFrom("candle_conflicts").selectAll().execute()).toHaveLength(1);
    const head = await db.selectFrom("candle_insert_heads").where("instrument_id", "=", instrumentId).selectAll().executeTakeFirstOrThrow();
    expect(head.revision).toBe("1");
    await insertCandleBatch(db, [{ instrumentId, candle: first }, { instrumentId, candle: first }]);
    expect((await db.selectFrom("candle_insert_heads").where("instrument_id", "=", instrumentId).selectAll().executeTakeFirstOrThrow()).revision).toBe("1");
  });

  it("serializes overlapping multi-instrument WS/REST writes and journals actual inserted ranges across months", async () => {
    const secondId = await ensureInstrument(db, { exchangeCode: "bybit", category: "linear", symbol: "BATCHSECONDUSDT" });
    await insertCandles(db, instrumentId, [candle(minute(0), "100")]);
    const previous = (await readGuards(db, [{ instrumentId, fromUnix: minute(0).getTime() / 1000,
      toUnix: minute(1).getTime() / 1000, previous: null }])).get(instrumentId)!.token;
    const results = await Promise.all([
      insertCandleBatch(db, [{ instrumentId: secondId, candle: candle(minute(3), "201") },
        { instrumentId, candle: candle(minute(0), "999") }, { instrumentId, candle: candle(minute(3), "103") }]),
      insertCandleBatch(db, [{ instrumentId, candle: candle(minute(3), "104") },
        { instrumentId: secondId, candle: candle(minute(3), "202") }]),
      insertCandles(db, secondId, [candle(minute(4), "205")]),
    ]);
    expect(results.reduce((n, r) => n + r.inserted, 0)).toBe(3);
    expect((await readCandles(db, { instrumentId, from: minute(0), to: minute(1) })).candles[0]?.close).toBe("100");
    const journal = await db.selectFrom("candle_insert_journal").where("instrument_id", "=", instrumentId).orderBy("revision").selectAll().execute();
    expect(journal).toHaveLength(2);
    expect(journal[1]).toMatchObject({ from_ts: minute(3), to_ts: minute(4) });
    const checked = await readGuards(db, [{ instrumentId, fromUnix: minute(0).getTime() / 1000,
      toUnix: minute(1).getTime() / 1000, previous }]);
    expect(checked.get(instrumentId)?.status).toBe("valid");
    const affected = await readGuards(db, [{ instrumentId, fromUnix: minute(3).getTime() / 1000,
      toUnix: minute(4).getTime() / 1000, previous }]);
    expect(affected.get(instrumentId)?.status).toBe("invalid");
    expect(await db.selectFrom("candles").where("instrument_id", "=", secondId).selectAll().execute()).toHaveLength(2);
  });

  it("cancels a blocked batch on the server and cannot commit later after timeout", async () => {
    await insertCandles(db, instrumentId, [candle(minute(0), "100")]);
    let unlock!: () => void;
    let locked!: () => void;
    const acquired = new Promise<void>((resolve) => { locked = resolve; });
    const release = new Promise<void>((resolve) => { unlock = resolve; });
    const blocker = db.transaction().execute(async (tx) => {
      await tx.selectFrom("instrument_watermarks").where("instrument_id", "=", instrumentId)
        .select("instrument_id").forUpdate().execute();
      locked(); await release;
    });
    await acquired;
    try {
      await expect(insertCandleBatch(db, [{ instrumentId, candle: candle(minute(1), "101") }], 50))
        .rejects.toThrow(/timeout|deadline/);
    } finally { unlock(); await blocker; }
    expect(await db.selectFrom("candles").where("instrument_id", "=", instrumentId).selectAll().execute()).toHaveLength(1);
    expect((await db.selectFrom("candle_insert_heads").where("instrument_id", "=", instrumentId).selectAll().executeTakeFirstOrThrow()).revision).toBe("1");
  });

  it("computes coverage intervals and gaps from stored candles", async () => {
    await insertCandles(db, instrumentId, [
      candle(minute(0), "100"),
      candle(minute(1), "101"),
      candle(minute(5), "105"),
      candle(minute(6), "106"),
    ]);
    const coverage = await getCoverage(db, instrumentId);
    expect(coverage.firstMinute).toBe(minute(0).toISOString());
    expect(coverage.lastMinute).toBe(minute(6).toISOString());
    expect(coverage.totalCandles).toBe(4);
    expect(coverage.intervals).toHaveLength(2);
    expect(coverage.intervals[0]).toEqual({
      start: minute(0).toISOString(),
      end: minute(1).toISOString(),
    });
    expect(coverage.gaps).toEqual([
      { start: minute(2).toISOString(), end: minute(4).toISOString() },
    ]);
  });

  it("reads candles by half-open range with cursor pagination", async () => {
    const batch: CandleInsert[] = [];
    for (let i = 0; i < 5; i += 1) {
      batch.push(candle(minute(i), String(100 + i)));
    }
    await insertCandles(db, instrumentId, batch);

    const firstPage = await readCandles(db, {
      instrumentId,
      from: minute(0),
      to: minute(5),
      limit: 3,
    });
    expect(firstPage.candles.map((c) => c.close)).toEqual(["100", "101", "102"]);
    expect(firstPage.nextCursorUnix).toBe(Math.floor(minute(2).getTime() / 1000));

    const secondPage = await readCandles(db, {
      instrumentId,
      from: minute(0),
      to: minute(5),
      limit: 3,
      afterUnix: firstPage.nextCursorUnix ?? undefined,
    });
    expect(secondPage.candles.map((c) => c.close)).toEqual(["103", "104"]);
    expect(secondPage.nextCursorUnix).toBeNull();
  });
});
