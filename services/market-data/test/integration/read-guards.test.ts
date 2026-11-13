import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it } from "vitest";
import type { Kysely } from "kysely";
import type { FastifyInstance } from "fastify";
import { buildApp } from "../../src/app.js";
import { createDb } from "../../src/db/db.js";
import { createMigrator } from "../../src/db/migrator.js";
import type { MarketDataDB } from "../../src/db/schema.js";
import { ensureInstrument } from "../../src/storage/instruments.js";
import { insertCandles, readCandles } from "../../src/storage/candles.js";
import { INSERT_JOURNAL_LIMIT, type ReadGuardResult, type ReadGuardToken } from "../../src/storage/read-guards.js";
import type { readTailBatch, TailBatchInput } from "../../src/storage/tail-batch.js";

const TEST_DATABASE_URL = process.env.TEST_DATABASE_URL;
const FROM = Date.UTC(2024, 5, 1) / 1000;
const minute = (n: number) => new Date((FROM + n * 60) * 1000);
const bar = (n: number, close = "1") => ({ openTime: minute(n), open: close, high: close, low: close, close, volume: "1", quoteTurnover: "1" });
const headers = { "x-api-key": "guard-test-key" };
type Body = Awaited<ReturnType<typeof readTailBatch>>;
function deferred() {
  let release = () => undefined as void;
  return { promise: new Promise<void>((resolve) => { release = resolve; }), resolve: () => release() };
}

describe.skipIf(!TEST_DATABASE_URL)("committed canonical insert read guards", () => {
  let db: Kysely<MarketDataDB>;
  let app: FastifyInstance;
  let id: number;
  beforeAll(async () => {
    db = createDb(TEST_DATABASE_URL!);
    const migration = await createMigrator(db).migrateToLatest();
    if (migration.error) throw new Error("guard test migration failed", { cause: migration.error });
    app = await buildApp({ db, apiKeys: [headers["x-api-key"]] });
  });
  beforeEach(async () => { id = await ensureInstrument(db, { exchangeCode: "bybit", category: "linear", symbol: "GUARDTESTUSDT" }); });
  afterEach(async () => {
    for (const table of ["candle_conflicts", "candles"] as const) await db.deleteFrom(table).where("instrument_id", "=", id).execute();
    await db.deleteFrom("instruments").where("id", "=", id).execute();
  });
  afterAll(async () => { await app.close(); await db.destroy(); });
  const request = (payload: TailBatchInput, authenticated = true) => app.inject({ method: "POST", url: "/v1/candles/tail-batch",
    ...(authenticated ? { headers } : {}), payload });
  async function observe(previous: ReadGuardToken | null, fromUnix = FROM, toUnix = FROM + 600) {
    const response = await request({ instrumentIds: [id], fromUnix: FROM, toUnix: FROM + 600,
      readGuards: [{ instrumentId: id, fromUnix, toUnix, previous }] });
    expect(response.statusCode).toBe(200);
    const instrument = response.json<Body>().instruments[0]!;
    expect(instrument.readGuard).toBeDefined();
    return instrument;
  }
  async function capture(): Promise<ReadGuardToken> {
    const observed = await observe(null);
    expect(observed.readGuard?.status).toBe("valid");
    expect(observed.readGuard?.token).not.toBeNull();
    return observed.readGuard!.token!;
  }

  it("captures baseline zero without creating candle history or a scan/bootstrap", async () => {
    const token = await capture();
    expect(token.revision).toBe("0");
    expect(await db.selectFrom("candles").where("instrument_id", "=", id).selectAll().execute()).toHaveLength(0);
    expect(await db.selectFrom("candle_insert_journal").where("instrument_id", "=", id).selectAll().execute()).toHaveLength(0);
  });
  it("invalidates an old hole while latest availability stays unchanged, even after an irrelevant insert", async () => {
    await insertCandles(db, id, [bar(0), bar(9)]);
    const token = await capture();
    await insertCandles(db, id, [bar(4)]);
    await insertCandles(db, id, [bar(-100)]);
    const observed = await observe(token);
    expect(observed.latestStoredOpenTimeUnix).toBe(FROM + 540);
    expect(observed.readGuard).toMatchObject({ status: "invalid", token: { revision: "3" } });
  });
  it("records newest inserts that fill an already-consumed missing tail", async () => {
    await insertCandles(db, id, [bar(0)]);
    const token = await capture();
    await insertCandles(db, id, [bar(9)]);
    expect((await observe(token)).readGuard?.status).toBe("invalid");
  });
  it("acknowledges irrelevant older inserts and newest appends outside the causal interval", async () => {
    await insertCandles(db, id, [bar(0)]);
    const token = await capture();
    await insertCandles(db, id, [bar(-100)]);
    const afterOld = (await observe(token)).readGuard!;
    expect(afterOld).toMatchObject({ status: "valid", token: { revision: "2" } });
    await insertCandles(db, id, [bar(10)]);
    expect((await observe(afterOld.token)).readGuard).toMatchObject({ status: "valid", token: { revision: "3" } });
  });
  it("uses actual RETURNING bounds and does not revise duplicate/conflict-only writes", async () => {
    await insertCandles(db, id, [bar(0)]);
    const token = await capture();
    await insertCandles(db, id, [bar(0)]);
    await insertCandles(db, id, [bar(0, "2")]);
    expect((await observe(token)).readGuard).toEqual({ status: "valid", token });
    await insertCandles(db, id, [bar(0), bar(10)]);
    expect((await observe(token)).readGuard).toMatchObject({ status: "valid", token: { revision: "2" } });
    const events = await db.selectFrom("candle_insert_journal").where("instrument_id", "=", id).orderBy("revision").selectAll().execute();
    expect(events).toHaveLength(2);
    expect(events[1]).toMatchObject({ from_ts: minute(10), to_ts: minute(11) });
  });
  it("conservatively invalidates the envelope of a mixed insertion batch", async () => {
    const token = await capture();
    await insertCandles(db, id, [bar(0), bar(9)]);
    expect((await observe(token, FROM + 300, FROM + 360)).readGuard?.status).toBe("invalid");
  });
  it("validates the same original token after historical pages and catches an insertion behind pagination", async () => {
    await insertCandles(db, id, [bar(0), bar(2), bar(4)]);
    const token = await capture();
    const first = await readCandles(db, { instrumentId: id, from: minute(0), to: minute(5), limit: 2 });
    expect(first.nextCursorUnix).toBe(FROM + 120);
    await insertCandles(db, id, [bar(1)]);
    const last = await readCandles(db, { instrumentId: id, from: minute(0), to: minute(5), afterUnix: first.nextCursorUnix!, limit: 2 });
    expect(last.candles.map((candle) => candle.openTimeUnix)).toEqual([FROM + 240]);
    expect((await observe(token, FROM, FROM + 300)).readGuard?.status).toBe("invalid");
  });
  it("commits candles, revision and journal atomically across serialized concurrent writers", async () => {
    await insertCandles(db, id, [bar(9)]);
    const token = await capture();
    const firstInserted = deferred(); const firstCommit = deferred();
    const secondInserted = deferred(); const secondCommit = deferred();
    const first = db.transaction().execute(async (tx) => {
      await insertCandles(tx, id, [bar(1)]); firstInserted.resolve(); await firstCommit.promise;
    });
    await firstInserted.promise;
    const second = db.transaction().execute(async (tx) => {
      await insertCandles(tx, id, [bar(2)]); secondInserted.resolve(); await secondCommit.promise;
    });
    try {
      const beforeCommit = await observe(token);
      expect(beforeCommit.readGuard).toEqual({ status: "valid", token });
      expect(beforeCommit.candles.map((candle) => candle.openTimeUnix)).toEqual([FROM + 540]);
      firstCommit.resolve(); await first; await secondInserted.promise;
      const betweenCommits = await observe(token);
      expect(betweenCommits.readGuard).toMatchObject({ status: "invalid", token: { revision: "2" } });
      expect(betweenCommits.candles.map((candle) => candle.openTimeUnix)).toEqual([FROM + 60, FROM + 540]);
    } finally { firstCommit.resolve(); secondCommit.resolve(); await Promise.all([first, second]); }
    expect((await observe(token)).readGuard).toMatchObject({ status: "invalid", token: { revision: "3" } });
    const events = await db.selectFrom("candle_insert_journal").where("instrument_id", "=", id).orderBy("revision").selectAll().execute();
    expect(events.map((event) => [event.revision, event.from_ts])).toEqual([["1", minute(9)], ["2", minute(1)], ["3", minute(2)]]);
  });
  it("rolls back revision/journal along with a failed canonical transaction", async () => {
    const token = await capture();
    await expect(db.transaction().execute(async (tx) => { await insertCandles(tx, id, [bar(1)]); throw new Error("rollback fixture"); })).rejects.toThrow("rollback fixture");
    expect((await observe(token)).readGuard).toEqual({ status: "valid", token });
    expect((await observe(null)).candles).toHaveLength(0);
  });
  it("retains exactly 1024 entries, validates r=F and returns unknown for r<F", async () => {
    const zero = await capture();
    await insertCandles(db, id, [bar(0)]);
    const one = await capture();
    for (let n = 1; n <= INSERT_JOURNAL_LIMIT; n++) await insertCandles(db, id, [bar(n)]);
    const head = await db.selectFrom("candle_insert_heads").where("instrument_id", "=", id).selectAll().executeTakeFirstOrThrow();
    expect(head).toMatchObject({ revision: "1025", pruned_through_revision: "1" });
    const events = await db.selectFrom("candle_insert_journal").where("instrument_id", "=", id).selectAll().execute();
    expect(events).toHaveLength(1024);
    expect((await observe(one, FROM - 60, FROM)).readGuard?.status).toBe("valid");
    expect((await observe(zero, FROM - 60, FROM)).readGuard?.status).toBe("unknown");
  }, 60_000);
  it("preserves epoch/token across an app/database-client restart", async () => {
    await insertCandles(db, id, [bar(0)]);
    const token = await capture();
    await app.close(); await db.destroy();
    db = createDb(TEST_DATABASE_URL!);
    const migrated = await createMigrator(db).migrateToLatest();
    if (migrated.error) throw new Error("restart migration failed", { cause: migrated.error });
    app = await buildApp({ db, apiKeys: [headers["x-api-key"]] });
    expect((await observe(token)).readGuard).toEqual({ status: "valid", token });
  });
  it("returns unknown for epoch mismatch, revision regression, missing/unready metadata or missing evidence", async () => {
    await insertCandles(db, id, [bar(0)]);
    const token = await capture();
    expect((await observe({ ...token, epoch: "different-source" })).readGuard?.status).toBe("unknown");
    expect((await observe({ ...token, revision: "99999999999999999999999999999999999999" })).readGuard?.status).toBe("unknown");
    await db.updateTable("candle_insert_heads").set({ ready: false }).where("instrument_id", "=", id).execute();
    expect((await observe(null)).readGuard).toEqual({ status: "unknown", token: null });
    await db.updateTable("candle_insert_heads").set({ ready: true }).where("instrument_id", "=", id).execute();
    await db.deleteFrom("candle_insert_journal").where("instrument_id", "=", id).execute();
    expect((await observe(null)).readGuard).toEqual({ status: "unknown", token: null });
    await db.deleteFrom("candle_insert_heads").where("instrument_id", "=", id).execute();
    expect((await observe(null)).readGuard).toEqual({ status: "unknown", token: null });
  });
  it("supports >90-day causal intervals, full safe integer bounds, empty prefixes and opt-in fields only", async () => {
    const token = await capture();
    expect((await observe(token, 0, Math.floor(Number.MAX_SAFE_INTEGER / 60) * 60)).readGuard?.status).toBe("valid");
    expect((await observe(token, FROM, FROM)).readGuard?.status).toBe("valid");
    const plain = await request({ instrumentIds: [id], fromUnix: FROM, toUnix: FROM + 60 });
    expect(plain.json<Body>().instruments[0]).not.toHaveProperty("readGuard");
    const unknown = await request({ instrumentIds: [id, 2_147_483_647], fromUnix: FROM, toUnix: FROM + 60,
      readGuards: [{ instrumentId: 2_147_483_647, fromUnix: FROM, toUnix: FROM + 60, previous: null }] });
    expect(unknown.json<Body>().instruments[1]?.readGuard).toEqual({ status: "unknown", token: null });
  });
  it("enforces auth and bounded/unique/request-scoped guard DTOs", async () => {
    const input: TailBatchInput = { instrumentIds: [id], fromUnix: FROM, toUnix: FROM + 60,
      readGuards: [{ instrumentId: id, fromUnix: FROM, toUnix: FROM + 60, previous: null }] };
    expect((await request(input, false)).statusCode).toBe(401);
    const base = input.readGuards![0]!;
    const bad = [
      [base, base], [{ ...base, instrumentId: id + 1 }], [{ ...base, fromUnix: -60 }],
      [{ ...base, fromUnix: FROM + 1 }], [{ ...base, fromUnix: FROM + 120 }], [{ ...base, toUnix: Number.MAX_SAFE_INTEGER + 1 }],
      [{ ...base, previous: { epoch: "x", revision: "-1" } }], [{ ...base, previous: { epoch: "x", revision: "1.5" } }],
      [{ ...base, previous: { epoch: "x", revision: "1".repeat(129) } }],
      Array.from({ length: 201 }, () => base),
    ];
    for (const readGuards of bad) {
      const response = await app.inject({ method: "POST", url: "/v1/candles/tail-batch", headers, payload: { ...input, readGuards } });
      expect(response.statusCode).toBe(400);
    }
  });
  it("returns unknown rather than accepting a missing source epoch", async () => {
    const identity = await db.selectFrom("candle_source_identity").selectAll().executeTakeFirstOrThrow();
    try {
      await db.deleteFrom("candle_source_identity").execute();
      const result: ReadGuardResult = (await observe(null)).readGuard!;
      expect(result).toEqual({ status: "unknown", token: null });
    } finally { await db.insertInto("candle_source_identity").values(identity).execute(); }
  });
});
