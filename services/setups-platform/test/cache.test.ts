 
import { afterAll, beforeAll, beforeEach, describe, expect, it } from "vitest";
import type { Kysely } from "kysely";
import { createDb } from "../src/db/db.js";
import { createMigrator } from "../src/db/migrator.js";
import type { SignalsDB } from "../src/db/schema.js";
import {
  cacheBulkLookup,
  cacheClearAll,
  cacheClearByInstrument,
  cacheLookup,
  cacheStats,
  cacheStore,
  canonicalCacheKey,
  type CacheQuery,
  type FinishedReplay,
} from "../src/replay/cache.js";
import { DEFAULT_SPIKE_PARAMS } from "../src/detector/price-volume-spike.js";
import {
  getNotificationMaxAgeSeconds,
  setNotificationMaxAgeSeconds,
  DEFAULT_NOTIFICATION_MAX_AGE_SECONDS,
} from "../src/settings.js";

const TEST_DATABASE_URL = process.env.TEST_DATABASE_URL;

const baseQuery: CacheQuery = {
  exchangeCode: "bybit",
  category: "linear",
  symbol: "TESTUSDT",
  algorithmId: "price_volume_spike",
  algorithmVersion: "1.0.0",
  params: DEFAULT_SPIKE_PARAMS,
  fromUnix: 1_700_000_000,
  toUnix: 1_700_003_600,
};

const sampleResult: FinishedReplay = {
  setups: [
    {
      algorithmId: "price_volume_spike",
      algorithmVersion: "1.0.0",
      openTimeUnix: 1_700_001_800,
      closeTimeUnix: 1_700_001_860,
      closePrice: "112",
      params: DEFAULT_SPIKE_PARAMS,
    },
  ],
  warmupMinutes: 43_199,
  processedMinutes: 60,
};

describe.skipIf(!TEST_DATABASE_URL)("replay cache", () => {
  let db: Kysely<SignalsDB>;

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
    await db.deleteFrom("replay_cache").execute();
    await db.deleteFrom("settings").execute();
  });

  it("stores and looks up an exact key", async () => {
    await cacheStore(db, baseQuery, sampleResult);
    const hit = await cacheLookup(db, baseQuery);
    expect(hit).not.toBeNull();
    expect(hit?.cacheKey).toBe(canonicalCacheKey(baseQuery));
    expect(hit?.setups).toEqual(sampleResult.setups);
    expect(hit?.warmupMinutes).toBe(sampleResult.warmupMinutes);
    expect(hit?.computedAt).toBeTruthy();
  });

  it("misses on any changed key dimension", async () => {
    await cacheStore(db, baseQuery, sampleResult);
    const changedParam: CacheQuery = {
      ...baseQuery,
      params: { ...DEFAULT_SPIKE_PARAMS, priceGrowthPct: "6" },
    };
    expect(await cacheLookup(db, changedParam)).toBeNull();
    expect(
      await cacheLookup(db, { ...baseQuery, algorithmVersion: "1.0.1" }),
    ).toBeNull();
    expect(await cacheLookup(db, { ...baseQuery, symbol: "OTHERUSDT" })).toBeNull();
    expect(await cacheLookup(db, { ...baseQuery, toUnix: baseQuery.toUnix + 60 })).toBeNull();
  });

  it("bulk lookup returns only hits keyed by canonical key", async () => {
    await cacheStore(db, baseQuery, sampleResult);
    const second = { ...baseQuery, symbol: "SECONDUSDT" };
    await cacheStore(db, second, { ...sampleResult, setups: [] });
    const third = { ...baseQuery, symbol: "THIRDUSDT" };
    const hits = await cacheBulkLookup(db, [baseQuery, second, third]);
    expect(hits.size).toBe(2);
    expect(hits.get(canonicalCacheKey(second))?.setups).toEqual([]);
    expect(hits.has(canonicalCacheKey(third))).toBe(false);
  });

  it("clears by instrument without touching other instruments", async () => {
    await cacheStore(db, baseQuery, sampleResult);
    await cacheStore(db, { ...baseQuery, symbol: "OTHERUSDT" }, sampleResult);
    const removed = await cacheClearByInstrument(db, {
      exchangeCode: baseQuery.exchangeCode,
      category: baseQuery.category,
      symbol: baseQuery.symbol,
    });
    expect(removed).toBe(1);
    expect(await cacheLookup(db, baseQuery)).toBeNull();
    expect(await cacheLookup(db, { ...baseQuery, symbol: "OTHERUSDT" })).not.toBeNull();
    const remaining = await cacheClearAll(db);
    expect(remaining).toBe(1);
  });

  it("reports cache stats with entry count and physical size", async () => {
    const empty = await cacheStats(db);
    expect(empty.entries).toBe(0);
    await cacheStore(db, baseQuery, sampleResult);
    const stats = await cacheStats(db);
    expect(stats.entries).toBe(1);
    expect(stats.bytes).toBeGreaterThan(0);
  });

  it("keeps the default notification age and stores changes", async () => {
    expect(await getNotificationMaxAgeSeconds(db)).toBe(
      DEFAULT_NOTIFICATION_MAX_AGE_SECONDS,
    );
    await setNotificationMaxAgeSeconds(db, 120);
    expect(await getNotificationMaxAgeSeconds(db)).toBe(120);
  });
});
