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
});
