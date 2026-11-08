import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it } from "vitest";
import { sql, type Kysely } from "kysely";
import { createDb } from "../src/db/db.js";
import { createMigrator } from "../src/db/migrator.js";
import type { SignalsDB } from "../src/db/schema.js";
import { buildApp } from "../src/app.js";
import { DEFAULT_SPIKE_PARAMS, SPIKE_ALGORITHM_ID, SPIKE_ALGORITHM_VERSION } from "../src/detector/price-volume-spike.js";
import type { SpikeSetup } from "../src/detector/price-volume-spike.js";
import { DEFAULT_REPLAY_ADMISSION_LIMITS, cancelReplayRun, enqueueReplayRuns, startReplayJobs } from "../src/replay/jobs.js";
import { cacheClearAll, cacheStats, cacheStore } from "../src/replay/cache.js";
import { ArrayCandleSource, type CandleSource } from "../src/replay/source.js";
import { setSetting } from "../src/settings.js";

const url = process.env.TEST_DATABASE_URL;
const input = { instruments: [{ exchange: "bybit", category: "linear", symbol: "ASYNCUSDT" }],
  fromUnix: 1_700_000_000, toUnix: 1_700_400_000, params: DEFAULT_SPIKE_PARAMS };
const marketData = { url: "http://127.0.0.1:1", apiKey: "test" };
const bars = Array.from({ length: 6000 }, (_, index) => ({ openTimeUnix: input.fromUnix + index * 60,
  close: "100", volume: "10", quoteTurnover: "1000" }));

describe.skipIf(!url)("persistent replay jobs", () => {
  let db: Kysely<SignalsDB>;
  let runtime: Awaited<ReturnType<typeof startReplayJobs>> | undefined;
  beforeAll(async () => {
    db = createDb(url!);
    const result = await createMigrator(db).migrateToLatest();
    if (result.error) throw new Error("test database migration failed", { cause: result.error });
  });
  beforeEach(async () => {
    await db.deleteFrom("replay_runs").execute();
    await db.deleteFrom("replay_cache").execute();
    await db.deleteFrom("settings").where("key", "in", ["replay", "replay-cache-generation"]).execute();
  });
  afterEach(async () => { await runtime?.stop(); runtime = undefined; });
  afterAll(async () => { await db.destroy(); });
  const get = (id: number) => db.selectFrom("replay_runs").selectAll().where("id", "=", id).executeTakeFirstOrThrow();
  async function until(id: number, status: string) {
    await expect.poll(async () => (await get(id)).status, { timeout: 10000, interval: 20 }).toBe(status);
    return get(id);
  }
  async function start(source: CandleSource = new ArrayCandleSource(bars)) {
    runtime = await startReplayJobs(db, marketData, { pollIntervalMs: 10, sourceForRun: () => Promise.resolve(source) });
  }

  it("streams pages, persists progress/results and serves cache on a later run", async () => {
    const [run] = (await enqueueReplayRuns(db, input));
    let reads = 0;
    const source = new ArrayCandleSource(bars);
    await start({ async read(query) { reads++; expect(query.limit).toBe(5000); return source.read(query); } });
    const done = await until(run!.id, "done");
    expect(reads).toBe(2);
    expect((done.warmup_minutes ?? 0) + (done.processed_minutes ?? 0)).toBe(6000);
    expect((await cacheStats(db)).entries).toBe(1);
    const [next] = (await enqueueReplayRuns(db, input));
    expect((await until(next!.id, "done")).cache_hit).toBe(true);
    expect(reads).toBe(2);
  });

  it("honors cancellation during a page fetch and never publishes its cache", async () => {
    const [run] = (await enqueueReplayRuns(db, input));
    let release!: () => void;
    const gate = new Promise<void>((resolve) => { release = resolve; });
    let reading = false;
    const source = new ArrayCandleSource(bars);
    await start({ async read(query) { reading = true; await gate; return source.read(query); } });
    await expect.poll(() => reading).toBe(true);
    expect((await cancelReplayRun(db, run!.id))!.status).toBe("cancel_requested");
    release();
    await until(run!.id, "cancelled");
    expect((await cacheStats(db)).entries).toBe(0);
  });

  it("does not restore a cache cleared while a page was in flight", async () => {
    const [run] = (await enqueueReplayRuns(db, input));
    let release!: () => void;
    const gate = new Promise<void>((resolve) => { release = resolve; });
    let reading = false;
    const source = new ArrayCandleSource(bars);
    await start({ async read(query) { reading = true; await gate; return source.read(query); } });
    await expect.poll(() => reading).toBe(true);
    await cacheClearAll(db);
    release();
    await until(run!.id, "done");
    expect((await cacheStats(db)).entries).toBe(0);
  });

  it("pauses queued work, cancels queued work and bounds pending submissions", async () => {
    await setSetting(db, "replay", { paused: true });
    const batch = { ...input, instruments: Array.from({ length: 100 }, () => input.instruments[0]!) };
    const queueLimits = { ...DEFAULT_REPLAY_ADMISSION_LIMITS, maxPendingRuns: 100 };
    const runs = (await enqueueReplayRuns(db, batch, queueLimits));
    await expect(enqueueReplayRuns(db, input, queueLimits)).rejects.toMatchObject({ resource: "queue_jobs", limit: 100, requested: 101 });
    await start();
    expect((await get(runs[0]!.id)).status).toBe("queued");
    expect((await cancelReplayRun(db, runs[0]!.id))!.status).toBe("cancelled");
    expect(await enqueueReplayRuns(db, input, queueLimits)).toHaveLength(1);
  });

  it("admits large instrument batches with default limits", async () => {
    const batch = { ...input, instruments: Array.from({ length: 150 }, () => input.instruments[0]!) };
    const runs = await enqueueReplayRuns(db, batch);
    expect(runs).toHaveLength(150);
    for (const run of runs) await cancelReplayRun(db, run.id);
    expect((await get(runs[0]!.id)).status).toBe("cancelled");
  });

  it("rejects excessive new work atomically but admits an exact cache hit", async () => {
    const limits = { ...DEFAULT_REPLAY_ADMISSION_LIMITS, maxWorkMinutes: 1 };
    await expect(enqueueReplayRuns(db, input, limits)).rejects.toMatchObject({
      resource: "work_minutes", limit: 1,
    });
    expect(await db.selectFrom("replay_runs").select((eb) => eb.fn.countAll().as("n")).executeTakeFirstOrThrow()).toMatchObject({ n: "0" });

    await cacheStore(db, {
      exchangeCode: input.instruments[0]!.exchange,
      category: input.instruments[0]!.category,
      symbol: input.instruments[0]!.symbol,
      algorithmId: SPIKE_ALGORITHM_ID,
      algorithmVersion: SPIKE_ALGORITHM_VERSION,
      params: input.params,
      fromUnix: input.fromUnix,
      toUnix: input.toUnix,
    }, { setups: [], warmupMinutes: 0, processedMinutes: 0 });
    expect(await enqueueReplayRuns(db, input, limits)).toHaveLength(1);
  });

  it("rechecks admission before computing when an admitted cache entry disappears", async () => {
    const limits = { ...DEFAULT_REPLAY_ADMISSION_LIMITS, maxWorkMinutes: 1 };
    await cacheStore(db, {
      exchangeCode: input.instruments[0]!.exchange,
      category: input.instruments[0]!.category,
      symbol: input.instruments[0]!.symbol,
      algorithmId: SPIKE_ALGORITHM_ID,
      algorithmVersion: SPIKE_ALGORITHM_VERSION,
      params: input.params,
      fromUnix: input.fromUnix,
      toUnix: input.toUnix,
    }, { setups: [], warmupMinutes: 0, processedMinutes: 0 });
    const [run] = await enqueueReplayRuns(db, input, limits);
    expect(run!.cache_hit).toBe(true);
    await cacheClearAll(db);
    let sourceReads = 0;
    runtime = await startReplayJobs(db, marketData, {
      pollIntervalMs: 10,
      admissionLimits: limits,
      sourceForRun: () => {
        sourceReads += 1;
        return Promise.resolve(new ArrayCandleSource(bars));
      },
    });
    const failed = await until(run!.id, "failed");
    expect(failed.error).toContain("work_minutes limit exceeded");
    expect(sourceReads).toBe(0);
  });

  it("serializes database reservations across concurrent submissions", async () => {
    const size = await sql<{ bytes: string }>`select pg_database_size(current_database())::text as bytes`.execute(db);
    const workMinutes = Math.ceil((input.toUnix - input.fromUnix) / 60)
      + input.params.baselineLongDays * 24 * 60;
    const limits = {
      ...DEFAULT_REPLAY_ADMISSION_LIMITS,
      estimatedBytesPerWorkMinute: 1,
      maxDatabaseBytes: Number(size.rows[0]!.bytes) + workMinutes + 4096,
    };

    const results = await Promise.allSettled([
      enqueueReplayRuns(db, input, limits),
      enqueueReplayRuns(db, input, limits),
    ]);

    expect(results.filter((result) => result.status === "fulfilled")).toHaveLength(1);
    expect(results.find((result) => result.status === "rejected"))
      .toMatchObject({ reason: { resource: "database_bytes" } });
  });

  it("rejects a replay when the configured database budget has no headroom", async () => {
    await expect(enqueueReplayRuns(db, input, { ...DEFAULT_REPLAY_ADMISSION_LIMITS, maxDatabaseBytes: 1 }))
      .rejects.toMatchObject({ resource: "database_bytes", limit: 1 });
    expect(await db.selectFrom("replay_runs").select((eb) => eb.fn.countAll().as("n")).executeTakeFirstOrThrow()).toMatchObject({ n: "0" });
  });

  it("does not execute a persisted unsupported algorithm version under a new engine", async () => {
    const [run] = (await enqueueReplayRuns(db, input));
    await db.updateTable("replay_runs").set({ algorithm_version: "0.0.0" }).where("id", "=", run!.id).execute();
    await start();
    await until(run!.id, "failed");
    expect((await get(run!.id)).error).toContain("unsupported_algorithm_version");
    expect((await cacheStats(db)).entries).toBe(0);
  });

  it("marks stale running jobs interrupted and acknowledges pending cancellation on startup", async () => {
    const runs = (await enqueueReplayRuns(db, { ...input, instruments: [input.instruments[0]!, input.instruments[0]!] }));
    await db.updateTable("replay_runs").set({ status: "running" }).where("id", "=", runs[0]!.id).execute();
    await db.updateTable("replay_runs").set({ status: "cancel_requested" }).where("id", "=", runs[1]!.id).execute();
    await start();
    expect((await get(runs[0]!.id)).status).toBe("failed");
    expect((await get(runs[0]!.id)).error).toContain("restart");
    expect((await get(runs[1]!.id)).status).toBe("cancelled");
  });

  it("pauses an active job at a page checkpoint and resumes without replaying that page", async () => {
    const [run] = (await enqueueReplayRuns(db, input));
    let release!: () => void;
    const gate = new Promise<void>((resolve) => { release = resolve; });
    let reads = 0;
    const source = new ArrayCandleSource(bars);
    await start({ async read(query) { reads++; await gate; return source.read(query); } });
    await expect.poll(() => reads).toBe(1);
    await setSetting(db, "replay", { paused: true });
    release();
    await expect.poll(async () => (await get(run!.id)).warmup_minutes).toBe(5000);
    expect(reads).toBe(1);
    expect((await get(run!.id)).status).toBe("running");
    await setSetting(db, "replay", { paused: false });
    await until(run!.id, "done");
    expect(reads).toBe(2);
  });

  it("stops a paused active worker and leaves an explicit interrupted result", async () => {
    const [run] = (await enqueueReplayRuns(db, input));
    let release!: () => void;
    const gate = new Promise<void>((resolve) => { release = resolve; });
    let reading = false;
    const source = new ArrayCandleSource(bars);
    await start({ async read(query) { reading = true; await gate; return source.read(query); } });
    await expect.poll(() => reading).toBe(true);
    await setSetting(db, "replay", { paused: true });
    release();
    await expect.poll(async () => (await get(run!.id)).warmup_minutes).toBe(5000);
    await runtime!.stop();
    runtime = undefined;
    const interrupted = await get(run!.id);
    expect(interrupted.status).toBe("failed");
    expect(interrupted.error).toContain("shutdown");
    expect((await cacheStats(db)).entries).toBe(0);
  });

  it("exposes async submission and authenticated cancellation without fetching market data", async () => {
    const app = await buildApp({ db, apiKeys: ["secret"], marketData });
    try {
      const response = await app.inject({ method: "POST", url: "/v1/replay-runs", headers: { "x-api-key": "secret" }, payload: { ...input, async: true } });
      expect(response.statusCode).toBe(202);
      const run = response.json<{ runs: { id: number; status: string; params: unknown }[] }>().runs[0]!;
      expect(run.status).toBe("queued");
      expect(run.params).toEqual(DEFAULT_SPIKE_PARAMS);
      expect((await app.inject({ method: "POST", url: `/v1/replay-runs/${run.id}/cancel` })).statusCode).toBe(401);
      const cancelled = await app.inject({ method: "POST", url: `/v1/replay-runs/${run.id}/cancel`, headers: { "x-api-key": "secret" } });
      expect(cancelled.json<{ run: { status: string } }>().run.status).toBe("cancelled");
      const invalid = await app.inject({ method: "POST", url: "/v1/replay-runs", headers: { "x-api-key": "secret" }, payload: { ...input, async: true, toUnix: input.fromUnix } });
      expect(invalid.statusCode).toBe(400);
      for (const range of [{ fromUnix: -1, toUnix: 60 }, { fromUnix: 253_402_300_700, toUnix: 253_402_300_800 }]) {
        const outsideBounds = await app.inject({ method: "POST", url: "/v1/replay-runs", headers: { "x-api-key": "secret" }, payload: { ...input, ...range, async: true } });
        expect(outsideBounds.statusCode).toBe(400);
      }
      // The original accepted request intentionally uses non-minute-aligned Unix seconds.
      expect(input.fromUnix % 60).not.toBe(0);
    } finally { await app.close(); }
  });

  it("applies configured work admission to synchronous and asynchronous API requests", async () => {
    const app = await buildApp({
      db,
      apiKeys: ["secret"],
      marketData,
      replayLimits: { ...DEFAULT_REPLAY_ADMISSION_LIMITS, maxWorkMinutes: 1 },
    });
    try {
      for (const async of [false, true]) {
        const response = await app.inject({
          method: "POST",
          url: "/v1/replay-runs",
          headers: { "x-api-key": "secret" },
          payload: { ...input, async },
        });
        expect(response.statusCode).toBe(429);
        expect(response.json()).toMatchObject({ resource: "work_minutes", limit: 1 });
      }
      const count = await db.selectFrom("replay_runs")
        .select((eb) => eb.fn.countAll<string>().as("n"))
        .executeTakeFirstOrThrow();
      expect(count.n).toBe("0");
    } finally {
      await app.close();
    }
  });

  it("releases synchronous reservations when catalog lookup fails", async () => {
    const app = await buildApp({
      db,
      apiKeys: ["secret"],
      marketData,
      replayLimits: { ...DEFAULT_REPLAY_ADMISSION_LIMITS, maxPendingRuns: 1 },
    });
    try {
      const failed = await app.inject({
        method: "POST",
        url: "/v1/replay-runs",
        headers: { "x-api-key": "secret" },
        payload: { ...input, async: false },
      });
      expect(failed.statusCode).toBe(500);
      const active = await db.selectFrom("replay_runs")
        .select((eb) => eb.fn.countAll<string>().as("n"))
        .where("status", "in", ["queued", "running", "cancel_requested"])
        .executeTakeFirstOrThrow();
      expect(active.n).toBe("0");

      const accepted = await app.inject({
        method: "POST",
        url: "/v1/replay-runs",
        headers: { "x-api-key": "secret" },
        payload: { ...input, async: true },
      });
      expect(accepted.statusCode).toBe(202);
    } finally {
      await app.close();
    }
  });

  it("returns optional trigger details and parameter snapshots for replay and live setups", async () => {
    const [run] = (await enqueueReplayRuns(db, input));
    const details = { growthPct: "12", volumeLongRatio: "3.01", volumeShortRatio: null,
      overLong: true, overShort: false, missingMinutes: 0 };
    const setup: SpikeSetup = { algorithmId: "price_volume_spike", algorithmVersion: "1.0.0",
      openTimeUnix: input.fromUnix, closeTimeUnix: input.fromUnix + 60, closePrice: "112", params: input.params };
    await db.updateTable("replay_runs").set({ setups: JSON.stringify([setup, { ...setup, details }]) as unknown as SpikeSetup[] })
      .where("id", "=", run!.id).execute();
    const inserted = await db.insertInto("live_setups").values({ exchange_code: "bybit", category: "linear", symbol: "ASYNC_DETAILS_TEST",
      algorithm_id: setup.algorithmId, algorithm_version: setup.algorithmVersion, params: input.params,
      open_time: new Date(input.fromUnix * 1000), close_time: new Date((input.fromUnix + 60) * 1000), close_price: "112", details })
      .returning("id").executeTakeFirstOrThrow();
    const app = await buildApp({ db, apiKeys: ["secret"], marketData });
    interface SetupResponse { setups: { symbol: string; algorithmVersion?: string; params?: unknown; details?: unknown }[] }
    try {
      const replay = await app.inject({ url: `/v1/replay-runs/${run!.id}`, headers: { "x-api-key": "secret" } });
      expect(replay.statusCode).toBe(200);
      const replaySetups = replay.json<SetupResponse>().setups;
      expect(replaySetups[0]!.details).toBeNull();
      expect(replaySetups[1]).toMatchObject({ details, params: input.params, algorithmVersion: "1.0.0" });
      const live = await app.inject({ url: "/v1/setups", headers: { "x-api-key": "secret" } });
      expect(live.statusCode).toBe(200);
      expect(live.json<SetupResponse>().setups.find((item) => item.symbol === "ASYNC_DETAILS_TEST"))
        .toMatchObject({ details, params: input.params, algorithmVersion: "1.0.0" });
      await db.updateTable("live_setups").set({ details: null }).where("id", "=", inserted.id).execute();
      const legacy = await app.inject({ url: "/v1/setups", headers: { "x-api-key": "secret" } });
      expect(legacy.json<SetupResponse>().setups.find((item) => item.symbol === "ASYNC_DETAILS_TEST")!.details).toBeNull();
    } finally {
      await app.close();
      await db.deleteFrom("live_setups").where("id", "=", inserted.id).execute();
    }
  });
});
