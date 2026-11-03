import { afterAll, beforeAll, describe, expect, it } from "vitest";
/* eslint-disable @typescript-eslint/no-unsafe-assignment, @typescript-eslint/no-unsafe-call, @typescript-eslint/no-unsafe-member-access --
   kysely/migration types resolve in tsc but not in the ESLint project service. */
import { sql, type Kysely } from "kysely";
import type { FastifyInstance } from "fastify";
import { createDb } from "../../src/db/db.js";
import { createMigrator } from "../../src/db/migrator.js";
import type { MarketDataDB } from "../../src/db/schema.js";
import { buildApp } from "../../src/app.js";
import { ensureInstrument } from "../../src/storage/instruments.js";
import { insertCandles, type CandleInsert } from "../../src/storage/candles.js";

const TEST_DATABASE_URL = process.env.TEST_DATABASE_URL;
const API_KEY = "test-api-key";

const minute = (offsetMinutes: number): Date =>
  new Date(Date.UTC(2024, 5, 30, 23, 57) + offsetMinutes * 60_000);

const unix = (date: Date): number => Math.floor(date.getTime() / 1000);

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

describe.skipIf(!TEST_DATABASE_URL)("market data v1 api", () => {
  let db: Kysely<MarketDataDB>;
  let app: FastifyInstance;
  let instrumentId: number;

  beforeAll(async () => {
    db = createDb(TEST_DATABASE_URL!);
    const migrator = createMigrator(db);
    const migrationResult = await migrator.migrateToLatest();
    if (migrationResult.error) {
      throw migrationResult.error;
    }
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
      symbol: "APIUSDT",
    });
    await insertCandles(db, instrumentId, [
      candle(minute(0), "100"),
      candle(minute(1), "101"),
      candle(minute(5), "105"),
      candle(minute(6), "106"),
    ]);
    app = await buildApp({ db, apiKeys: [API_KEY], openApiEnabled: false });
  });

  afterAll(async () => {
    await app.close();
    await db.destroy();
  });

  it("keeps /health public", async () => {
    const response = await app.inject({ method: "GET", url: "/health" });
    expect(response.statusCode).toBe(200);
  });

  it("rejects v1 requests without an api key", async () => {
    const response = await app.inject({ method: "GET", url: "/v1/instruments" });
    expect(response.statusCode).toBe(401);
  });

  it("rejects v1 requests with a wrong api key", async () => {
    const response = await app.inject({
      method: "GET",
      url: "/v1/instruments",
      headers: { "x-api-key": "wrong-key" },
    });
    expect(response.statusCode).toBe(401);
  });

  it("lists instruments with exchange identity for authorized clients", async () => {
    const response = await app.inject({
      method: "GET",
      url: "/v1/instruments",
      headers: { "x-api-key": API_KEY },
    });
    expect(response.statusCode).toBe(200);
    const body = response.json<{
      instruments: {
        id: number;
        exchange: string;
        category: string;
        symbol: string;
        status: string;
      }[];
    }>();
    const found = body.instruments.find(
      (instrument) => instrument.symbol === "APIUSDT",
    );
    expect(found).toMatchObject({
      exchange: "bybit",
      category: "linear",
      status: "active",
    });
    expect(found?.id).toBe(instrumentId);
  });

  it("returns coverage with unix-second intervals and gaps", async () => {
    const response = await app.inject({
      method: "GET",
      url: `/v1/instruments/${instrumentId}/coverage`,
      headers: { "x-api-key": API_KEY },
    });
    expect(response.statusCode).toBe(200);
    const body = response.json<{
      firstMinuteUnix: number;
      lastMinuteUnix: number;
      totalCandles: number;
      intervals: { startUnix: number; endUnix: number }[];
      gaps: { startUnix: number; endUnix: number }[];
    }>();
    expect(body.firstMinuteUnix).toBe(unix(minute(0)));
    expect(body.lastMinuteUnix).toBe(unix(minute(6)));
    expect(body.totalCandles).toBe(4);
    expect(body.intervals).toHaveLength(2);
    expect(body.gaps).toEqual([
      { startUnix: unix(minute(2)), endUnix: unix(minute(4)) },
    ]);
  });

  it("reads candles as decimal strings with unix-second timestamps", async () => {
    const response = await app.inject({
      method: "GET",
      url: `/v1/candles?instrumentId=${instrumentId}&fromUnix=${unix(minute(0))}&toUnix=${unix(minute(7))}`,
      headers: { "x-api-key": API_KEY },
    });
    expect(response.statusCode).toBe(200);
    const body = response.json<{
      candles: {
        instrumentId: number;
        openTimeUnix: number;
        close: string;
        volume: string;
        quoteTurnover: string;
        source: string;
      }[];
      nextCursorUnix: number | null;
    }>();
    expect(body.candles.map((candleRow) => candleRow.close)).toEqual([
      "100",
      "101",
      "105",
      "106",
    ]);
    expect(body.candles[0]?.openTimeUnix).toBe(unix(minute(0)));
    expect(body.candles[0]?.volume).toBe("1.5");
    expect(body.candles[0]?.quoteTurnover).toBe("1500.25");
    expect(body.candles[0]?.source).toBe("bybit");
    expect(body.nextCursorUnix).toBeNull();
  });

  it("paginates candles with an explicit cursor", async () => {
    const first = await app.inject({
      method: "GET",
      url: `/v1/candles?instrumentId=${instrumentId}&fromUnix=${unix(minute(0))}&toUnix=${unix(minute(7))}&limit=2`,
      headers: { "x-api-key": API_KEY },
    });
    const firstBody = first.json<{
      candles: { close: string }[];
      nextCursorUnix: number | null;
    }>();
    expect(firstBody.candles.map((candleRow) => candleRow.close)).toEqual([
      "100",
      "101",
    ]);
    expect(firstBody.nextCursorUnix).toBe(unix(minute(1)));

    const second = await app.inject({
      method: "GET",
      url: `/v1/candles?instrumentId=${instrumentId}&fromUnix=${unix(minute(0))}&toUnix=${unix(minute(7))}&limit=2&afterUnix=${firstBody.nextCursorUnix}`,
      headers: { "x-api-key": API_KEY },
    });
    const secondBody = second.json<{
      candles: { close: string }[];
      nextCursorUnix: number | null;
    }>();
    expect(secondBody.candles.map((candleRow) => candleRow.close)).toEqual([
      "105",
      "106",
    ]);
    expect(secondBody.nextCursorUnix).toBeNull();
  });

  it("rejects limits above the contract maximum", async () => {
    const response = await app.inject({
      method: "GET",
      url: `/v1/candles?instrumentId=${instrumentId}&fromUnix=${unix(minute(0))}&toUnix=${unix(minute(7))}&limit=5001`,
      headers: { "x-api-key": API_KEY },
    });
    expect(response.statusCode).toBe(400);
  });
});
