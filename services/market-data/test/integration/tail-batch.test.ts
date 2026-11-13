import { afterAll, beforeAll, describe, expect, it } from "vitest";
import { sql, type Kysely } from "kysely";
import type { FastifyInstance } from "fastify";
import { createDb } from "../../src/db/db.js";
import { createMigrator } from "../../src/db/migrator.js";
import type { MarketDataDB } from "../../src/db/schema.js";
import { buildApp } from "../../src/app.js";
import { ensureInstrument } from "../../src/storage/instruments.js";
import { insertCandles } from "../../src/storage/candles.js";
import type { readTailBatch } from "../../src/storage/tail-batch.js";

const URL = process.env.TEST_DATABASE_URL;
const FROM = Date.UTC(2024, 5, 1) / 1000;
const headers = { "x-api-key": "tail-test-key" };
type Body = Awaited<ReturnType<typeof readTailBatch>>;

describe.skipIf(!URL)("bounded saved-only tail-batch API", () => {
  let db: Kysely<MarketDataDB>;
  let app: FastifyInstance;
  let id: number;
  const ids: number[] = [];
  beforeAll(async () => {
    db = createDb(URL!);
    const migration = await createMigrator(db).migrateToLatest();
    if (migration.error) throw new Error("test migration failed", { cause: migration.error });
    id = await ensureInstrument(db, { exchangeCode: "bybit", category: "linear", symbol: "TAILAPIUSDT" });
    ids.push(id);
    await insertCandles(db, id, [0, 1, 3, 4].map((n) => ({ openTime: new Date((FROM + n * 60) * 1000),
      open: "0.100000000000000000001", high: "2", low: "0.1", close: "1", volume: "1", quoteTurnover: "2" })));
    app = await buildApp({ db, apiKeys: [headers["x-api-key"]] });
  });
  afterAll(async () => {
    await app.close();
    for (const table of ["candle_conflicts", "candles"] as const) await db.deleteFrom(table).where("instrument_id", "in", ids).execute();
    await db.deleteFrom("instruments").where("id", "in", ids).execute();
    await db.destroy();
  });
  const request = (payload: object) => app.inject({ method: "POST", url: "/v1/candles/tail-batch", headers, payload });

  it("separates latest from requested-prefix continuity, preserves decimals and performs no loading", async () => {
    const jobsBefore = await db.selectFrom("backfill_jobs").select("id").execute();
    const response = await request({ instrumentIds: [id, 2_147_483_647], fromUnix: FROM, toUnix: FROM + 360 });
    expect(response.statusCode).toBe(200);
    const body = response.json<Body>();
    expect(body.instruments[0]).toMatchObject({ instrumentId: id, status: "known", latestStoredOpenTimeUnix: FROM + 240,
      contiguousThroughOpenTimeUnix: FROM + 60, missingRanges: [{ fromUnix: FROM + 120, toUnix: FROM + 180 }, { fromUnix: FROM + 300, toUnix: FROM + 360 }] });
    expect(body.instruments[0]?.candles[0]?.open).toBe("0.100000000000000000001");
    expect(body.instruments[1]).toMatchObject({ status: "not_found", latestStoredOpenTimeUnix: null,
      contiguousThroughOpenTimeUnix: null, candles: [], missingRanges: [{ fromUnix: FROM, toUnix: FROM + 360 }] });
    expect(await db.selectFrom("backfill_jobs").select("id").execute()).toEqual(jobsBefore);
    expect(app.swagger().paths).toHaveProperty("/v1/candles/tail-batch");
  });
  it("requires auth and rejects duplicate IDs, misalignment, excessive range and ID count", async () => {
    expect((await app.inject({ method: "POST", url: "/v1/candles/tail-batch", payload: {} })).statusCode).toBe(401);
    for (const payload of [
      { instrumentIds: [id, id], fromUnix: FROM, toUnix: FROM + 60 },
      { instrumentIds: [id], fromUnix: FROM + 1, toUnix: FROM + 60 },
      { instrumentIds: [id], fromUnix: FROM, toUnix: FROM + 1860 },
      { instrumentIds: [id], fromUnix: FROM, toUnix: FROM },
      { instrumentIds: Array.from({ length: 201 }, (_, n) => n + 1), fromUnix: FROM, toUnix: FROM + 60 },
    ]) expect((await request(payload)).statusCode).toBe(400);
  });
  it("returns all 6000 candles at the supported maximum, never a truncated success", async () => {
    const exchange = await db.selectFrom("exchanges").where("code", "=", "bybit").select("id").executeTakeFirstOrThrow();
    const instruments = await db.insertInto("instruments").values(Array.from({ length: 200 }, (_, n) => ({
      exchange_id: exchange.id, category: "linear", symbol: `TAILMAX${n}USDT`, status: "active" as const,
    }))).returning("id").execute();
    ids.push(...instruments.map((row) => row.id));
    for (const instrument of instruments) await insertCandles(db, instrument.id, Array.from({ length: 30 }, (_, n) => ({
      openTime: new Date((FROM + n * 60) * 1000), open: "1", high: "1", low: "1", close: "1", volume: "1", quoteTurnover: "1",
    })));
    const response = await request({ instrumentIds: instruments.map((row) => row.id), fromUnix: FROM, toUnix: FROM + 1800,
      readGuards: instruments.map((row) => ({ instrumentId: row.id, fromUnix: 0, toUnix: FROM + 1800, previous: null })),
    });
    expect(response.statusCode).toBe(200);
    const body = response.json<Body>();
    expect(body.instruments).toHaveLength(200);
    expect(body.instruments.reduce((sum, row) => sum + row.candles.length, 0)).toBe(6000);
    expect(body.instruments.every((row) => row.missingRanges.length === 0 && row.contiguousThroughOpenTimeUnix === FROM + 1740)).toBe(true);
    expect(body.instruments.every((row) => row.readGuard?.status === "valid" && row.readGuard.token?.revision === "1")).toBe(true);
  });
  it("rejects an oversized saved payload before returning any partial tail", async () => {
    try {
      await sql`UPDATE candles SET source = repeat('x', 2200000) WHERE instrument_id = ${id}`.execute(db);
      const response = await request({ instrumentIds: [id], fromUnix: FROM, toUnix: FROM + 360 });
      expect(response.statusCode).toBe(413);
      expect(response.json<{ error: string }>().error).toContain("byte limit");
    } finally { await db.updateTable("candles").set({ source: "bybit" }).where("instrument_id", "=", id).execute(); }
  });
});
