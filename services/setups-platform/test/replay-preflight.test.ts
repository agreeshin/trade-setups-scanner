import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import type { Kysely } from "kysely";
import type { FastifyInstance } from "fastify";
import { createDb } from "../src/db/db.js";
import { createMigrator } from "../src/db/migrator.js";
import type { SignalsDB } from "../src/db/schema.js";
import { buildApp } from "../src/app.js";
import { DEFAULT_SPIKE_PARAMS, SPIKE_ALGORITHM_ID, SPIKE_ALGORITHM_VERSION } from "../src/detector/price-volume-spike.js";
import { cacheStore } from "../src/replay/cache.js";
import { replayPreflight, type CoverageRange } from "../src/replay/preflight.js";
import { ReplayQualityTracker } from "../src/replay/quality.js";
import { saveAlgorithm } from "../src/algorithms.js";
import * as algorithms from "../src/algorithms.js";

const databaseUrl = process.env.TEST_DATABASE_URL;
const headers = { "x-api-key": "preflight-test" };
const marketData = { url: "http://127.0.0.1:1", apiKey: "unused" };
const params = { ...DEFAULT_SPIKE_PARAMS, baselineLongDays: 1, baselineShortDays: 1 };
const input = { instruments: [{ exchange: "bybit", category: "linear", symbol: "PREFLIGHTUSDT" }],
  fromUnix: 1_700_000_040, toUnix: 1_700_000_640, algorithmId: SPIKE_ALGORITHM_ID, params };
type Report = Awaited<ReturnType<typeof replayPreflight>>;
const requestUrl = (request: string | URL | Request) => typeof request === "string" ? request : request instanceof URL ? request.href : request.url;

function coverage(request: string, empty = false): CoverageRange {
  const url = new URL(request);
  const fromUnix = Math.ceil(Number(url.searchParams.get("fromUnix")) / 60) * 60;
  const toUnix = Math.ceil(Number(url.searchParams.get("toUnix")) / 60) * 60;
  const splitUnix = Math.ceil(Number(url.searchParams.get("splitUnix")) / 60) * 60;
  const expectedMinutes = (toUnix - fromUnix) / 60;
  return { instrumentId: Number(url.pathname.split("/")[3]), fromUnix, toUnix, splitUnix, expectedMinutes,
    presentMinutes: empty ? 0 : expectedMinutes, missingMinutes: empty ? expectedMinutes : 0,
    warmupExpectedMinutes: (splitUnix - fromUnix) / 60, warmupPresentMinutes: empty ? 0 : (splitUnix - fromUnix) / 60,
    signalExpectedMinutes: (toUnix - splitUnix) / 60, signalPresentMinutes: empty ? 0 : (toUnix - splitUnix) / 60,
    gaps: empty ? [{ fromUnix, toUnix }] : [], gapCount: empty ? 1 : 0, offset: 0, limit: 50,
    checkedAt: "2026-11-04T00:00:00.000Z" };
}

describe.skipIf(!databaseUrl)("read-only replay preflight", () => {
  let db: Kysely<SignalsDB>;
  let app: FastifyInstance;
  let onClosed: (() => void) | undefined;
  beforeAll(async () => {
    db = createDb(databaseUrl!);
    const migrated = await createMigrator(db).migrateToLatest();
    if (migrated.error) throw new Error("preflight migration failed", { cause: migrated.error });
    app = await buildApp({ db, marketData, apiKeys: [headers["x-api-key"]] });
    app.addHook("onRequest", (_request, reply, done) => {
      if (onClosed) reply.raw.once("close", onClosed);
      done();
    });
  });
  beforeEach(async () => {
    await db.deleteFrom("replay_runs").execute();
    await db.deleteFrom("replay_cache").execute();
    await db.deleteFrom("settings").where("key", "=", `algorithm:${SPIKE_ALGORITHM_ID}`).execute();
  });
  afterEach(() => { onClosed = undefined; vi.restoreAllMocks(); });
  afterAll(async () => { await app.close(); await db.destroy(); });
  const preview = (payload = input) => app.inject({ method: "POST", url: "/v1/replay-preflight", headers, payload });
  async function seedCache() {
    const tracker = new ReplayQualityTracker(input.fromUnix, input.toUnix, 1440);
    tracker.finish();
    await cacheStore(db, { exchangeCode: "bybit", category: "linear", symbol: "PREFLIGHTUSDT",
      algorithmId: SPIKE_ALGORITHM_ID, algorithmVersion: SPIKE_ALGORITHM_VERSION, params,
      fromUnix: input.fromUnix, toUnix: input.toUnix }, {
      setups: [], warmupMinutes: 0, processedMinutes: 0, quality: tracker.quality,
      computedAt: new Date("2026-01-01T00:00:00Z"),
    });
  }

  it("serves original cached quality without history access or any persisted mutation", async () => {
    await seedCache();
    const fetch = vi.spyOn(globalThis, "fetch").mockRejectedValue(new Error("history offline"));
    const cacheBefore = await db.selectFrom("replay_cache").selectAll().execute();
    const settingsBefore = await db.selectFrom("settings").selectAll().execute();
    const response = await preview();
    expect(response.statusCode).toBe(200);
    const report = response.json<Report>();
    expect(report).toMatchObject({ canRun: true, requiredHistoryMinutes: 1440,
      results: [{ status: "cached", cacheHit: true, computedAt: "2026-01-01T00:00:00.000Z", quality: { state: "no_eligible_data" } }] });
    expect(report.request).toEqual({ ...input, algorithmVersion: SPIKE_ALGORITHM_VERSION, async: true });
    expect(fetch).not.toHaveBeenCalled();
    expect(await db.selectFrom("replay_runs").selectAll().execute()).toEqual([]);
    expect(await db.selectFrom("replay_cache").selectAll().execute()).toEqual(cacheBefore);
    expect(await db.selectFrom("settings").selectAll().execute()).toEqual(settingsBefore);
  });

  it("distinguishes missing coverage from unavailable history and preserves mixed cache hits", async () => {
    await seedCache();
    const fetch = vi.spyOn(globalThis, "fetch").mockResolvedValue(new Response("offline", { status: 503 }));
    const payload = { ...input, instruments: [...input.instruments, { exchange: "bybit", category: "linear", symbol: "OTHERUSDT" }] };
    const report = (await preview(payload)).json<Report>();
    expect(report.canRun).toBe(false);
    expect(report.results.map((item) => item.status)).toEqual(["cached", "unavailable"]);
    expect(report.results[1]!.coverage).toBeUndefined();
    expect(fetch).toHaveBeenCalledTimes(1);
  });

  it("previews empty input and warmup without launching or forbidding an explicit partial run", async () => {
    const urls: string[] = [];
    vi.spyOn(globalThis, "fetch").mockImplementation((request) => {
      const url = requestUrl(request); urls.push(url);
      return Promise.resolve(Response.json(url.endsWith("/v1/instruments")
        ? { instruments: [{ id: 7, ...input.instruments[0], status: "Trading" }] } : coverage(url, true)));
    });
    const report = (await preview()).json<Report>();
    expect(report.canRun).toBe(true);
    expect(report.results[0]).toMatchObject({ status: "ready", coverage: { expectedMinutes: 1450,
      presentMinutes: 0, missingMinutes: 1450, warmupExpectedMinutes: 1440, signalExpectedMinutes: 10, gapCount: 1 } });
    expect(urls).toHaveLength(2);
    expect(urls[1]).toContain("/coverage-range?");
    expect(await db.selectFrom("replay_runs").selectAll().execute()).toEqual([]);
  });

  it("reports missing instruments and malformed range responses instead of successful empty results", async () => {
    vi.spyOn(globalThis, "fetch").mockResolvedValue(Response.json({ instruments: [] }));
    expect((await preview()).json<Report>().results[0]!.status).toBe("not_found");
    vi.spyOn(globalThis, "fetch").mockImplementation((request) => Promise.resolve(Response.json(requestUrl(request).endsWith("/v1/instruments")
      ? { instruments: [{ id: 7, ...input.instruments[0], status: "Trading" }] } : {})));
    const report = (await preview()).json<Report>();
    expect(report.canRun).toBe(false);
    expect(report.results[0]!.error).toBe("invalid_coverage_response");
  });

  it("bounds concurrent history reads and preserves selection order", async () => {
    const instruments = Array.from({ length: 9 }, (_, i) => ({ exchange: "bybit", category: "linear", symbol: `P${i}USDT` }));
    let active = 0; let maximum = 0;
    vi.spyOn(globalThis, "fetch").mockImplementation(async (request) => {
      const url = requestUrl(request);
      if (url.endsWith("/v1/instruments")) return Response.json({ instruments: instruments.map((item, i) => ({ id: i + 1, ...item, status: "Trading" })) });
      active++; maximum = Math.max(maximum, active);
      await new Promise((resolve) => setTimeout(resolve, 5));
      active--;
      return Response.json(coverage(url));
    });
    const report = (await preview({ ...input, instruments })).json<Report>();
    expect(report.canRun).toBe(true);
    expect(report.results.map((item) => item.symbol)).toEqual(instruments.map((item) => item.symbol));
    expect(maximum).toBe(4);
    expect(active).toBe(0);
  });

  it("runs the confirmed parameter snapshot and rejects a changed algorithm version", async () => {
    await seedCache();
    const request = (await preview()).json<Report>().request;
    await saveAlgorithm(db, SPIKE_ALGORITHM_ID, { ...params, priceGrowthPct: "99" }, true);
    const launched = await app.inject({ method: "POST", url: "/v1/replay-runs", headers, payload: request });
    expect(launched.statusCode).toBe(202);
    const run = await db.selectFrom("replay_runs").selectAll().executeTakeFirstOrThrow();
    expect(run.params).toEqual(params);
    expect((await app.inject({ method: "POST", url: "/v1/replay-runs", headers,
      payload: { ...request, algorithmVersion: "obsolete" } })).statusCode).toBe(400);
  });

  it("aborts active history reads and starts no further work after client cancellation", async () => {
    const instruments = Array.from({ length: 9 }, (_, i) => ({ exchange: "bybit", category: "linear", symbol: `C${i}USDT` }));
    const controller = new AbortController();
    let reads = 0;
    let active = 0;
    vi.spyOn(globalThis, "fetch").mockImplementation(async (request, options) => {
      if (requestUrl(request).endsWith("/v1/instruments")) return Response.json({ instruments: instruments.map((item, i) => ({ id: i + 1, ...item, status: "Trading" })) });
      reads++;
      if (options?.signal?.aborted) throw new Error("cancelled");
      active++;
      queueMicrotask(() => controller.abort());
      try {
        return await new Promise<Response>((_resolve, reject) => {
          options?.signal?.addEventListener("abort", () => reject(new Error("cancelled")), { once: true });
        });
      } finally { active--; }
    });
    const report = await replayPreflight(db, { ...input, instruments, algorithmVersion: SPIKE_ALGORITHM_VERSION }, marketData, controller.signal);
    expect(report.canRun).toBe(false);
    expect(reads).toBeLessThanOrEqual(4);
    expect(active).toBe(0);
    expect(await db.selectFrom("replay_runs").selectAll().execute()).toEqual([]);
  });

  it("validates request bounds and authorization without consulting history", async () => {
    const fetch = vi.spyOn(globalThis, "fetch").mockRejectedValue(new Error("must not fetch"));
    expect((await app.inject({ method: "POST", url: "/v1/replay-preflight", payload: input })).statusCode).toBe(401);
    expect((await preview({ ...input, toUnix: input.fromUnix })).statusCode).toBe(400);
    expect((await preview({ ...input, algorithmId: "unknown" })).statusCode).toBe(400);
    expect((await preview({ ...input, instruments: Array.from({ length: 101 }, () => input.instruments[0]!) })).statusCode).toBe(400);
    expect(fetch).not.toHaveBeenCalled();
  });

  it("honors disconnect during parameter preparation before any history requests", async () => {
    const original = algorithms.getAlgorithm;
    const actualFetch = globalThis.fetch;
    let release!: () => void;
    let entered!: () => void;
    let prepared = false;
    const gate = new Promise<void>((resolve) => { release = resolve; });
    const started = new Promise<void>((resolve) => { entered = resolve; });
    const closed = new Promise<void>((resolve) => { onClosed = resolve; });
    vi.spyOn(algorithms, "getAlgorithm").mockImplementation(async (...args) => {
      entered(); await gate;
      const algorithm = await original(...args);
      prepared = true;
      return algorithm;
    });
    const fetch = vi.spyOn(globalThis, "fetch").mockRejectedValue(new Error("must not consult history"));
    const origin = await app.listen({ host: "127.0.0.1", port: 0 });
    const controller = new AbortController();
    const pending = actualFetch(`${origin}/v1/replay-preflight`, { method: "POST",
      headers: { ...headers, "content-type": "application/json" }, body: JSON.stringify(input), signal: controller.signal }).catch(() => undefined);
    try {
      await started;
      controller.abort();
      await pending;
      await closed;
    } finally { release(); }
    await expect.poll(() => prepared).toBe(true);
    await new Promise<void>((resolve) => setImmediate(resolve));
    expect(fetch).not.toHaveBeenCalled();
  });
});
