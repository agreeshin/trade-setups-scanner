 
import { afterAll, beforeAll, beforeEach, describe, expect, it } from "vitest";
import { mkdtemp, readFile, rm } from "node:fs/promises";
import { tmpdir } from "node:os";
import { join } from "node:path";
import { sql, type Kysely } from "kysely";
import { createDb } from "../../src/db/db.js";
import { createMigrator } from "../../src/db/migrator.js";
import type { MarketDataDB } from "../../src/db/schema.js";
import { ensureInstrument } from "../../src/storage/instruments.js";
import { insertCandles, type CandleInsert } from "../../src/storage/candles.js";
import { exportMarketData, type MarketExportManifest } from "../../src/export/market-export.js";

const TEST_DATABASE_URL = process.env.TEST_DATABASE_URL;
const MINUTE = 60_000;
const T0 = Date.UTC(2024, 5, 1);
const minute = (offset: number): Date => new Date(T0 + offset * MINUTE);

function candle(openTime: Date, close: string): CandleInsert {
  return {
    openTime,
    open: close,
    high: close,
    low: close,
    close,
    volume: "1.5",
    quoteTurnover: "150.25",
  };
}

describe.skipIf(!TEST_DATABASE_URL)("market data export", () => {
  let db: Kysely<MarketDataDB>;
  let outDir: string;

  beforeAll(async () => {
    db = createDb(TEST_DATABASE_URL!);
    const migrator = createMigrator(db);
    const migrationResult = await migrator.migrateToLatest();
    if (migrationResult.error) {
      throw new Error(`migration failed: ${JSON.stringify(migrationResult.error)}`);
    }
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
    outDir = await mkdtemp(join(tmpdir(), "market-export-"));
  });

  afterAll(async () => {
    if (outDir) {
      await rm(outDir, { recursive: true, force: true });
    }
    await db.destroy();
  });

  it("writes a standalone csv and manifest with exact decimals", async () => {
    const instrumentId = await ensureInstrument(db, {
      exchangeCode: "bybit",
      category: "linear",
      symbol: "EXPUSDT",
    });
    await insertCandles(db, instrumentId, [
      candle(minute(0), "100"),
      candle(minute(1), "101.5"),
      candle(minute(5), "105"),
    ]);

    const manifest = await exportMarketData(db, outDir);
    expect(manifest.formatVersion).toBe(1);
    expect(manifest.time.semantics).toBe("unix-seconds-utc");
    expect(manifest.units.volume).toBe("decimal-string-base-asset");

    const entry = manifest.instruments.find(
      (item) => item.symbol === "EXPUSDT",
    );
    expect(entry).toMatchObject({
      exchange: "bybit",
      category: "linear",
      totalCandles: 3,
    });
    expect(entry?.firstMinuteUnix).toBe(Math.floor(minute(0).getTime() / 1000));
    expect(entry?.lastMinuteUnix).toBe(Math.floor(minute(5).getTime() / 1000));

    const csv = await readFile(`${outDir}/candles.csv`, "utf8");
    const lines = csv.trim().split("\n");
    expect(lines[0]).toBe(
      "exchange,category,symbol,openTimeUnix,open,high,low,close,volume,quoteTurnover",
    );
    expect(lines).toHaveLength(4);
    expect(lines[1]).toBe(
      `bybit,linear,EXPUSDT,${Math.floor(minute(0).getTime() / 1000)},100,100,100,100,1.5,150.25`,
    );
    expect(lines[2]).toContain(",101.5,");

    const manifestJson = JSON.parse(
      await readFile(`${outDir}/manifest.json`, "utf8"),
    ) as MarketExportManifest;
    expect(manifestJson.instruments).toHaveLength(1);
  });

  it("includes instruments without history in the manifest", async () => {
    await ensureInstrument(db, {
      exchangeCode: "bybit",
      category: "linear",
      symbol: "EMPTYUSDT",
    });
    const manifest = await exportMarketData(db, outDir);
    const entry = manifest.instruments.find((item) => item.symbol === "EMPTYUSDT");
    expect(entry).toMatchObject({
      totalCandles: 0,
      firstMinuteUnix: null,
      lastMinuteUnix: null,
    });
    const csv = await readFile(`${outDir}/candles.csv`, "utf8");
    expect(csv.trim().split("\n")).toHaveLength(1);
  });
});
