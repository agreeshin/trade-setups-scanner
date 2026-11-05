import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import type { Kysely } from "kysely";
import type { FastifyInstance } from "fastify";
import { createDb } from "../src/db/db.js";
import { createMigrator } from "../src/db/migrator.js";
import type { SignalsDB } from "../src/db/schema.js";
import { buildApp } from "../src/app.js";
import { DEFAULT_SPIKE_PARAMS } from "../src/detector/price-volume-spike.js";
import { cancelReplayRun, enqueueReplayRuns, startReplayJobs } from "../src/replay/jobs.js";
import { cacheClearAll } from "../src/replay/cache.js";
import { ArrayCandleSource, type CandleSource } from "../src/replay/source.js";
import { ReplayQualityTracker, saveQualityPage } from "../src/replay/quality.js";

const url = process.env.TEST_DATABASE_URL;
const start = 1_700_000_040;
const params = { ...DEFAULT_SPIKE_PARAMS, baselineLongDays: 1, baselineShortDays: 1 };
const input = { instruments: [{ exchange: "quality-test", category: "linear", symbol: "QUSDT" }],
  fromUnix: start, toUnix: start + 600, params };
const headers = { "x-api-key": "quality-test" };
const marketData = { url: "http://127.0.0.1:1", apiKey: "unused" };
const bars = Array.from({ length: 1450 }, (_, i) => ({ openTimeUnix: start + (i - 1440) * 60,
  close: "1", volume: "1", quoteTurnover: "1" }));

describe.skipIf(!url)("persisted replay quality", () => {
  let db: Kysely<SignalsDB>;
  let app: FastifyInstance;
  let runtime: Awaited<ReturnType<typeof startReplayJobs>> | undefined;
  beforeAll(async () => {
    db = createDb(url!);
    const migration = await createMigrator(db).migrateToLatest();
    if (migration.error) throw new Error("quality migration failed", { cause: migration.error });
    app = await buildApp({ db, marketData, apiKeys: [headers["x-api-key"]] });
    await app.ready();
  });
  beforeEach(async () => {
    await db.deleteFrom("replay_runs").execute();
    await db.deleteFrom("replay_cache").execute();
    await db.deleteFrom("settings").where("key", "in", ["replay", "replay-cache-generation"]).execute();
  });
  afterEach(async () => { await runtime?.stop(); runtime = undefined; vi.restoreAllMocks(); });
  afterAll(async () => { await app.close(); await db.destroy(); });
  const get = (id: number) => db.selectFrom("replay_runs").selectAll().where("id", "=", id).executeTakeFirstOrThrow();
  async function until(id: number, status: string) {
    await expect.poll(async () => (await get(id)).status, { timeout: 10000, interval: 20 }).toBe(status);
    return get(id);
  }
  async function launch(source: CandleSource) {
    const [run] = (await enqueueReplayRuns(db, input));
    runtime = await startReplayJobs(db, marketData, { pollIntervalMs: 10, sourceForRun: () => Promise.resolve(source) });
    return run!.id;
  }

  it("persists complete zero-signal quality, coalesces pages and preserves cache provenance after clear", async () => {
    const source = new ArrayCandleSource(bars);
    let reads = 0;
    const id = await launch({ async read(query) { reads++; return source.read({ ...query, limit: 100 }); } });
    const done = await until(id, "done");
    expect(done.quality).toMatchObject({ state: "complete", requestedMinutes: 10, evaluatedMinutes: 10, skippedMinutes: 0, unassessedMinutes: 0 });
    expect(done.setups).toEqual([]);
    const intervals = await app.inject({ url: `/v1/replay-runs/${id}/quality-intervals`, headers });
    expect(intervals.json()).toMatchObject({ available: true, total: 1,
      intervals: [{ fromUnix: start, toUnix: start + 600, reason: "evaluated" }] });
    const initialReads = reads;
    const [hit] = (await enqueueReplayRuns(db, input));
    const cached = await until(hit!.id, "done");
    expect(cached.cache_hit).toBe(true);
    expect(reads).toBe(initialReads);
    expect(cached.quality).toEqual(done.quality);
    expect(cached.quality_source_run_id).toBe(id);
    expect(cached.quality_computed_at).toEqual(done.quality_computed_at);
    await cacheClearAll(db);
    expect((await app.inject({ url: `/v1/replay-runs/${hit!.id}/quality-intervals`, headers })).json()).toEqual(intervals.json());
    await db.deleteFrom("replay_runs").where("id", "=", id).execute();
    expect((await app.inject({ url: `/v1/replay-runs/${hit!.id}/quality-intervals`, headers })).json()).toMatchObject({ available: false });
    expect((await get(hit!.id)).quality).toEqual(done.quality);
  });

  it("records missing output candles separately from downstream warmup skips and keeps partial cache", async () => {
    const source = new ArrayCandleSource(bars.filter((bar) => bar.openTimeUnix < start || bar.openTimeUnix >= start + 180));
    const id = await launch(source);
    const done = await until(id, "done");
    expect(done.quality).toMatchObject({ state: "no_eligible_data", skippedMinutes: 10, unassessedMinutes: 0,
      reasonCounts: { missing_candle: 3, recent_gap: 7 } });
    const [hit] = (await enqueueReplayRuns(db, input));
    expect((await until(hit!.id, "done")).quality).toEqual(done.quality);
  });

  it("keeps committed quality prefix on source failure without declaring the tail missing", async () => {
    let reads = 0;
    const id = await launch({ read() {
      if (reads++ > 0) return Promise.reject(new Error("fixture source failure"));
      return Promise.resolve({ bars: bars.slice(0, 1443), nextCursorUnix: start + 120 });
    } });
    const failed = await until(id, "failed");
    expect(failed.quality).toMatchObject({ state: "in_progress", evaluatedMinutes: 3, unassessedMinutes: 7,
      reasonCounts: { missing_candle: 0 } });
    expect(await db.selectFrom("replay_cache").selectAll().execute()).toEqual([]);
  });

  it("keeps unvisited tail on cancellation while waiting for the next source page", async () => {
    let release!: () => void;
    const gate = new Promise<void>((resolve) => { release = resolve; });
    let reads = 0;
    const id = await launch({ async read() {
      if (reads++ === 0) return { bars: bars.slice(0, 1443), nextCursorUnix: start + 120 };
      await gate;
      return { bars: [], nextCursorUnix: null };
    } });
    try {
      await expect.poll(() => reads).toBe(2);
      await cancelReplayRun(db, id);
    } finally { release(); }
    const cancelled = await until(id, "cancelled");
    expect(cancelled.quality).toMatchObject({ state: "in_progress", evaluatedMinutes: 3, unassessedMinutes: 7,
      reasonCounts: { missing_candle: 0 } });
  });

  it("paginates exact intervals, protects access and marks legacy quality unknown", async () => {
    const [run] = (await enqueueReplayRuns(db, input));
    const endpoint = `/v1/replay-runs/${run!.id}/quality-intervals`;
    expect((await app.inject({ url: endpoint, headers })).json()).toMatchObject({ available: false });
    const tracker = new ReplayQualityTracker(start, start + 600, 1440);
    for (let i = 0; i < 10; i++) {
      tracker.observe(start + i * 60, i % 2 ? { status: "evaluated", continuousMinutes: 1440 }
        : { status: "warmup", continuousMinutes: 1, skipReason: "recent_gap" });
      await db.transaction().execute((tx) => saveQualityPage(tx, run!.id, tracker.quality, tracker.drain()));
    }
    const page = await app.inject({ url: `${endpoint}?offset=7&limit=2`, headers });
    expect(page.json()).toMatchObject({ total: 10, offset: 7, limit: 2, available: true,
      intervals: [{ fromUnix: start + 420, reason: "evaluated" }, { fromUnix: start + 480, reason: "recent_gap" }] });
    expect((await app.inject({ url: endpoint })).statusCode).toBe(401);
    expect((await app.inject({ url: `${endpoint}?limit=501`, headers })).statusCode).toBe(400);
    expect((await app.inject({ url: "/v1/replay-runs/2147483647/quality-intervals", headers })).statusCode).toBe(404);
    const detail = (await app.inject({ url: `/v1/replay-runs/${run!.id}`, headers })).json<{ run: { quality: unknown } }>();
    expect(detail.run.quality).toEqual(tracker.quality);
  });

  it.each(["failure", "cancel", "complete"] as const)("sync API preserves the same checkpoint contract on %s", async (outcome) => {
    let reads = 0;
    const first = bars.slice(0, 1443).map((bar) => bar.openTimeUnix === start
      ? { ...bar, close: "2", quoteTurnover: "1000" } : bar);
    vi.spyOn(globalThis, "fetch").mockImplementation(async (request) => {
      const requestUrl = typeof request === "string" ? request : request instanceof URL ? request.href : request.url;
      if (requestUrl.endsWith("/v1/instruments")) return Response.json({ instruments: [
        { id: 1, ...input.instruments[0], status: "Trading" },
      ] });
      if (reads++ === 0) {
        if (outcome === "cancel") {
          const run = await db.selectFrom("replay_runs").select("id").orderBy("id", "desc").executeTakeFirstOrThrow();
          await cancelReplayRun(db, run.id);
        }
        return Response.json({ candles: first, nextCursorUnix: start + 120 });
      }
      if (outcome === "failure") throw new Error("fixture sync source failure");
      return Response.json({ candles: [], nextCursorUnix: null });
    });
    const response = await app.inject({ method: "POST", url: "/v1/replay-runs", headers, payload: input });
    expect(response.statusCode).toBe(200);
    const id = response.json<{ runs: { id: number }[] }>().runs[0]!.id;
    const run = await get(id);
    expect(run.status).toBe(outcome === "failure" ? "failed" : outcome === "cancel" ? "cancelled" : "done");
    expect(response.json<{ runs: unknown[] }>().runs[0]).toMatchObject({ status: run.status, setupCount: 1,
      warmupMinutes: run.warmup_minutes, processedMinutes: run.processed_minutes, quality: run.quality });
    expect(run.setups?.length).toBe(1);
    expect(run.quality).toMatchObject({ evaluatedMinutes: 3,
      unassessedMinutes: outcome === "complete" ? 0 : 7,
      reasonCounts: { missing_candle: outcome === "complete" ? 7 : 0 } });
    const intervals = (await app.inject({ url: `/v1/replay-runs/${id}/quality-intervals`, headers })).json<{ total: number }>();
    expect(intervals.total).toBe(outcome === "complete" ? 2 : 1);
  });
});
