import { describe, expect, it } from "vitest";
import { loadConfig } from "../src/config.js";

const required = {
  DATABASE_URL: "postgresql://example/signals",
  MARKET_DATA_URL: "http://market-data",
  MARKET_DATA_API_KEY: "key",
};

describe("platform resource config", () => {
  it("loads bounded defaults and explicit replay limits", () => {
    expect(loadConfig(required).replayLimits).toMatchObject({ maxPendingRuns: 10_000, maxWorkMinutes: 5_270_400_000 });
    expect(loadConfig({ ...required, REPLAY_MAX_PENDING_RUNS: "7", REPLAY_MAX_DATABASE_BYTES: "9000" }).replayLimits)
      .toMatchObject({ maxPendingRuns: 7, maxDatabaseBytes: 9000 });
  });

  it("fails closed on invalid replay limits", () => {
    expect(() => loadConfig({ ...required, REPLAY_MAX_WORK_MINUTES: "0" })).toThrow("REPLAY_MAX_WORK_MINUTES");
    expect(() => loadConfig({ ...required, REPLAY_MAX_DATABASE_BYTES: "not-a-number" })).toThrow("REPLAY_MAX_DATABASE_BYTES");
  });
});
