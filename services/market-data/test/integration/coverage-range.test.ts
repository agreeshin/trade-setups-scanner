import { afterAll, beforeAll, describe, expect, it, vi } from "vitest";
import type { FastifyInstance } from "fastify";
import { sql, type Kysely } from "kysely";
import { createDb } from "../../src/db/db.js";
import { createMigrator } from "../../src/db/migrator.js";
import type { MarketDataDB } from "../../src/db/schema.js";
import { buildApp } from "../../src/app.js";
import { ensureInstrument } from "../../src/storage/instruments.js";
import { insertCandles } from "../../src/storage/candles.js";
import { COVERAGE_MIN_UNIX, COVERAGE_MAX_UNIX, getCoverage, getCoverageRange, type CoverageRange } from "../../src/storage/coverage.js";

const databaseUrl = process.env.TEST_DATABASE_URL;
const base = Date.UTC(2024, 5, 1) / 1000;
const headers = { "x-api-key": "coverage-range-test" };
const candle = (time: number) => ({ openTime: new Date(time * 1000), open: "100", high: "100", low: "100", close: "100", volume: "1", quoteTurnover: "100" });

describe.skipIf(!databaseUrl)("bounded coverage range", () => {
  let db: Kysely<MarketDataDB>;
  let app: FastifyInstance;
  let sparse: number;
  let empty: number;
  let many: number;
  let preEpoch: number;
  const ids: number[] = [];
  beforeAll(async () => {
    db = createDb(databaseUrl!);
    const result = await createMigrator(db).migrateToLatest();
    if (result.error) throw new Error("test migration failed", { cause: result.error });
    for (const symbol of ["RANGE_SPARSE", "RANGE_EMPTY", "RANGE_MANY", "RANGE_PRE_EPOCH"]) {
      ids.push(await ensureInstrument(db, { exchangeCode: "bybit", category: "linear", symbol }));
    }
    [sparse, empty, many, preEpoch] = ids as [number, number, number, number];
    await db.deleteFrom("candles").where("instrument_id", "in", ids).execute();
    await insertCandles(db, sparse, [-1, 1, 2, 5, 8, 10].map((minute) => candle(base + minute * 60)));
    await insertCandles(db, many, Array.from({ length: 130 }, (_, index) => candle(base + index * 120)));
    await insertCandles(db, preEpoch, [candle(-60), candle(0)]);
    app = await buildApp({ db, apiKeys: [headers["x-api-key"]], openApiEnabled: false });
  });
  afterAll(async () => {
    await app?.close();
    if (db) {
      if (ids.length) {
        await db.deleteFrom("candles").where("instrument_id", "in", ids).execute();
        await db.deleteFrom("instruments").where("id", "in", ids).execute();
      }
      await db.destroy();
    }
  });
  const url = (id: number, params: Record<string, number> = {}) => `/v1/instruments/${id}/coverage-range?${new URLSearchParams(
    Object.entries({ fromUnix: base, toUnix: base + 600, splitUnix: base + 300, ...params }).map(([key, value]) => [key, String(value)]),
  ).toString()}`;

  it("counts exact minute rows and leading, internal and trailing half-open gaps", async () => {
    const response = await app.inject({ url: url(sparse), headers });
    expect(response.statusCode).toBe(200);
    const coverage = response.json<CoverageRange>();
    expect(coverage).toMatchObject({ instrumentId: sparse, fromUnix: base, toUnix: base + 600, splitUnix: base + 300,
      expectedMinutes: 10, presentMinutes: 4, missingMinutes: 6,
      warmupExpectedMinutes: 5, warmupPresentMinutes: 2, signalExpectedMinutes: 5, signalPresentMinutes: 2,
      gapCount: 4, offset: 0, limit: 50,
      gaps: [[0, 1], [3, 5], [6, 8], [9, 10]].map(([from, to]) => ({ fromUnix: base + from! * 60, toUnix: base + to! * 60 })),
    });
    expect(new Date(coverage.checkedAt).toISOString()).toBe(coverage.checkedAt);
  });

  it("rounds all boundaries upward and permits an empty rounded range", async () => {
    const response = await app.inject({ url: url(sparse, { fromUnix: base + 1, toUnix: base + 599, splitUnix: base + 301 }), headers });
    expect(response.statusCode).toBe(200);
    expect(response.json<CoverageRange>()).toMatchObject({ fromUnix: base + 60, toUnix: base + 600, splitUnix: base + 360,
      expectedMinutes: 9, presentMinutes: 4, missingMinutes: 5, warmupExpectedMinutes: 5, warmupPresentMinutes: 3,
      signalExpectedMinutes: 4, signalPresentMinutes: 1, gapCount: 3 });
    const roundedEmpty = await app.inject({ url: url(sparse, { fromUnix: base + 1, toUnix: base + 2, splitUnix: base + 1 }), headers });
    expect(roundedEmpty.statusCode).toBe(200);
    expect(roundedEmpty.json<CoverageRange>()).toMatchObject({ expectedMinutes: 0, presentMinutes: 0, missingMinutes: 0, gapCount: 0, gaps: [] });
  });

  it("supports pre-epoch candles and split endpoints", async () => {
    const response = await app.inject({ url: url(preEpoch, { fromUnix: -120, toUnix: 120, splitUnix: 0 }), headers });
    expect(response.statusCode).toBe(200);
    expect(response.json<CoverageRange>()).toMatchObject({ expectedMinutes: 4, presentMinutes: 2, missingMinutes: 2,
      warmupPresentMinutes: 1, signalPresentMinutes: 1, gaps: [{ fromUnix: -120, toUnix: -60 }, { fromUnix: 60, toUnix: 120 }] });
    for (const splitUnix of [base, base + 600]) {
      const edge = await app.inject({ url: url(sparse, { splitUnix }), headers });
      expect(edge.statusCode).toBe(200);
      expect(edge.json<CoverageRange>()).toMatchObject(splitUnix === base
        ? { warmupExpectedMinutes: 0, warmupPresentMinutes: 0, signalPresentMinutes: 4 }
        : { signalExpectedMinutes: 0, signalPresentMinutes: 0, warmupPresentMinutes: 4 });
    }
  });

  it("returns one huge gap for existing empty history without generating missing minutes", async () => {
    const response = await app.inject({ url: url(empty, { fromUnix: COVERAGE_MIN_UNIX, toUnix: COVERAGE_MAX_UNIX, splitUnix: 0 }), headers });
    expect(response.statusCode).toBe(200);
    expect(response.json<CoverageRange>()).toMatchObject({ presentMinutes: 0, gapCount: 1,
      expectedMinutes: (COVERAGE_MAX_UNIX - COVERAGE_MIN_UNIX) / 60,
      gaps: [{ fromUnix: COVERAGE_MIN_UNIX, toUnix: COVERAGE_MAX_UNIX }] });
  });

  it("paginates all gaps without changing aggregate counts", async () => {
    const firstResponse = await app.inject({ url: url(many, { toUnix: base + 260 * 60, limit: 100 }), headers });
    expect(firstResponse.statusCode).toBe(200);
    const first = firstResponse.json<CoverageRange>();
    expect(first).toMatchObject({ gapCount: 130, presentMinutes: 130, missingMinutes: 130 });
    expect(first.gaps).toHaveLength(100);
    const second = (await app.inject({ url: url(many, { toUnix: base + 260 * 60, offset: 100, limit: 100 }), headers })).json<CoverageRange>();
    expect(second.gaps).toHaveLength(30);
    expect(second.gapCount).toBe(first.gapCount);
    expect(second.gaps[0]).toEqual({ fromUnix: base + 201 * 60, toUnix: base + 202 * 60 });
    const pastEnd = (await app.inject({ url: url(many, { toUnix: base + 260 * 60, offset: 1000 }), headers })).json<CoverageRange>();
    expect(pastEnd.gaps).toEqual([]);
    expect(pastEnd.gapCount).toBe(130);
  });

  it("requires auth, distinguishes unknown instruments and rejects invalid parameters", async () => {
    expect((await app.inject({ url: url(sparse) })).statusCode).toBe(401);
    expect((await app.inject({ url: url(2_147_483_647), headers })).statusCode).toBe(404);
    expect((await app.inject({ url: url(-1), headers })).statusCode).toBe(400);
    for (const params of [{ fromUnix: base + 600 }, { toUnix: base - 1 }, { splitUnix: base - 1 },
      { splitUnix: base + 601 }, { fromUnix: 1.5 }, { toUnix: COVERAGE_MAX_UNIX + 1 },
      { fromUnix: COVERAGE_MIN_UNIX - 1 }, { limit: 101 }, { limit: 0 }, { offset: -1 }]) {
      expect((await app.inject({ url: url(sparse, params), headers })).statusCode).toBe(400);
    }
    expect((await app.inject({ url: `/v1/instruments/${sparse}/coverage-range`, headers })).statusCode).toBe(400);
  });

  it("works in a read-only transaction and leaves original coverage behavior unchanged", async () => {
    const before = await getCoverage(db, sparse);
    const fetch = vi.spyOn(globalThis, "fetch");
    try {
      await db.transaction().execute(async (tx) => {
        await sql`SET TRANSACTION READ ONLY`.execute(tx);
        const result = await getCoverageRange(tx, sparse, { fromUnix: base, toUnix: base + 600, splitUnix: base + 300, limit: 2 });
        expect(result?.gaps).toHaveLength(2);
        expect(result?.gapCount).toBe(4);
      });
      expect(fetch).not.toHaveBeenCalled();
    } finally { fetch.mockRestore(); }
    expect(await getCoverage(db, sparse)).toEqual(before);
    const old = await app.inject({ url: `/v1/instruments/${sparse}/coverage`, headers });
    expect(old.statusCode).toBe(200);
    expect(old.json<{ totalCandles: number }>().totalCandles).toBe(6);
  });
});
