import { describe, expect, it } from "vitest";
import { loadConfig } from "../src/config.js";

describe("market-data resource config", () => {
  it("loads bounded defaults and explicit backfill limits", () => {
    expect(loadConfig({ DATABASE_URL: "postgresql://example/market" }).backfillLimits)
      .toMatchObject({ maxPendingJobs: 100, maxWorkMinutes: 2_635_200 });
    expect(loadConfig({ DATABASE_URL: "postgresql://example/market", BACKFILL_MAX_PENDING_JOBS: "9", MARKET_DATA_MAX_DATABASE_BYTES: "8000" }).backfillLimits)
      .toMatchObject({ maxPendingJobs: 9, maxDatabaseBytes: 8000 });
  });

  it("fails closed on invalid backfill limits", () => {
    expect(() => loadConfig({ DATABASE_URL: "postgresql://example/market", BACKFILL_MAX_PENDING_JOBS: "-1" })).toThrow("BACKFILL_MAX_PENDING_JOBS");
    expect(() => loadConfig({ DATABASE_URL: "postgresql://example/market", BACKFILL_ESTIMATED_BYTES_PER_MINUTE: "1.5" })).toThrow("BACKFILL_ESTIMATED_BYTES_PER_MINUTE");
  });
  it("loads the single shared Bybit budget and rejects unsafe/unbounded source settings", () => {
    const config = loadConfig({ DATABASE_URL: "postgresql://example/market" });
    expect(config.bybit).toMatchObject({ requestTimeoutMs: 5000,
      gate: { maxPerWindow: 150, minIntervalMs: 34, freshBurst: 6 }, fresh: { maxSymbols: 4096, ws: { shardSymbols: 200 } } });
    for (const [key, value] of [["BYBIT_REST_MAX_RPS", "121"], ["BYBIT_WS_SHARD_SYMBOLS", "201"],
      ["BYBIT_FRESH_MAX_SYMBOLS", "0"], ["BYBIT_REQUEST_TIMEOUT_MS", "Infinity"]]) {
      expect(() => loadConfig({ DATABASE_URL: "postgresql://example/market", [key!]: value })).toThrow(key);
    }
  });
});
