import { afterAll, beforeAll, describe, expect, it } from "vitest";
import type { FastifyInstance } from "fastify";
import type { Kysely } from "kysely";
import { buildApp } from "../../src/app.js";
import { createDb } from "../../src/db/db.js";
import { createMigrator } from "../../src/db/migrator.js";
import type { MarketDataDB } from "../../src/db/schema.js";
import { requestBackfill } from "../../src/ingest/backfill.js";
import { topUpBackfillPlans } from "../../src/ingest/plans.js";
import { ensureInstrument } from "../../src/storage/instruments.js";

const url = process.env.TEST_DATABASE_URL;
const headers = { "x-api-key": "management-test-key" };
const fromUnix = 1704067200;
interface Job { id: number; instrumentId: number; fromUnix: number; toUnix: number; status: string; requestedByCaller: boolean; checkpointUnix: number | null }

describe.skipIf(!url)("history management API", () => {
  let db: Kysely<MarketDataDB>;
  let app: FastifyInstance;
  let instrumentId: number;
  const planInstrumentIds: number[] = [];

  beforeAll(async () => {
    if (!/test/i.test(new URL(url!).pathname)) throw new Error("Management tests require an isolated test database");
    db = createDb(url!);
    const migration = await createMigrator(db).migrateToLatest();
    if (migration.error) throw new Error("Management test migration failed", { cause: migration.error });
    instrumentId = await ensureInstrument(db, { exchangeCode: "bybit", category: "linear", symbol: `MGMT${Date.now()}USDT` });
    planInstrumentIds.push(
      await ensureInstrument(db, { exchangeCode: "bybit", category: "linear", symbol: `PLNBASE${Date.now()}A` }),
      await ensureInstrument(db, { exchangeCode: "bybit", category: "linear", symbol: `PLNBASE${Date.now()}B` }),
    );
    app = await buildApp({ db, apiKeys: [headers["x-api-key"], "other-management-key"], openApiEnabled: true });
  });
  afterAll(async () => {
    await app?.close();
    if (db) {
      const ids = [instrumentId, ...planInstrumentIds].filter((id) => typeof id === "number");
      await db.deleteFrom("backfill_plan_items").execute();
      await db.deleteFrom("backfill_plans").execute();
      for (const id of ids) {
        await db.deleteFrom("collection_targets").where("instrument_id", "=", id).execute();
        await db.deleteFrom("backfill_jobs").where("instrument_id", "=", id).execute();
      }
      await db.deleteFrom("instruments").where("id", "in", ids).execute();
      await db.destroy();
    }
  });

  it("protects every management route before validation", async () => {
    for (const method of ["GET", "POST", "PUT"] as const) {
      const paths = method === "GET" ? ["/v1/backfill-jobs", "/v1/collection-targets", "/v1/instruments/listing", "/v1/backfill-plans"] : method === "POST" ? ["/v1/backfill-jobs", "/v1/backfill-jobs/1/cancel", "/v1/backfill-jobs/batch", "/v1/backfill-plans", "/v1/backfill-plans/1/cancel"] : ["/v1/collection-targets", "/v1/collection-targets/1"];
      for (const path of paths) {
        for (const auth of [{}, { "x-api-key": "invalid" }]) {
          expect((await app.inject({ method, url: path, headers: auth })).statusCode).toBe(401);
        }
      }
    }
  });

  it("rejects invalid ids, limits and non-closed/non-minute/oversized ranges", async () => {
    for (const path of ["/v1/backfill-jobs?limit=201", "/v1/backfill-jobs?beforeId=0", "/v1/backfill-jobs?instrumentId=2147483648", "/v1/collection-targets?limit=1001"]) {
      expect((await app.inject({ method: "GET", url: path, headers })).statusCode).toBe(400);
    }
    const bodies = [
      { instrumentId: 0, fromUnix, toUnix: fromUnix + 60 },
      { instrumentId, fromUnix: fromUnix + 1, toUnix: fromUnix + 60 },
      { instrumentId, fromUnix, toUnix: fromUnix },
      { instrumentId, fromUnix, toUnix: fromUnix - 60 },
      { instrumentId, fromUnix: 0, toUnix: 1830 * 86400 + 60 },
      { instrumentId, fromUnix, toUnix: Math.ceil(Date.now() / 60000) * 60 + 60 },
    ];
    for (const payload of bodies) expect((await app.inject({ method: "POST", url: "/v1/backfill-jobs", headers, payload })).statusCode).toBe(400);
    expect((await app.inject({ method: "POST", url: "/v1/backfill-jobs/nope/cancel", headers })).statusCode).toBe(400);
    expect((await app.inject({ method: "PUT", url: `/v1/collection-targets/${instrumentId}`, headers, payload: {} })).statusCode).toBe(400);
    expect((await app.inject({ method: "POST", url: "/v1/backfill-jobs", headers, payload: { instrumentId: 2147483647, fromUnix, toUnix: fromUnix + 60 } })).statusCode).toBe(404);
    expect((await app.inject({ method: "POST", url: "/v1/backfill-jobs/2147483647/cancel", headers })).statusCode).toBe(404);
    expect((await app.inject({ method: "PUT", url: "/v1/collection-targets/2147483647", headers, payload: { enabled: true } })).statusCode).toBe(404);
  });

  it("enqueues without fetching, reuses covering work and preserves other interests on cancel", async () => {
    const payload = { instrumentId, fromUnix, toUnix: fromUnix + 120 };
    const first = await app.inject({ method: "POST", url: "/v1/backfill-jobs", headers, payload });
    expect(first.statusCode).toBe(202);
    const job = first.json<{ job: Job }>().job;
    expect(job).toMatchObject({ ...payload, status: "queued", checkpointUnix: null, requestedByCaller: true });
    const duplicate = await app.inject({ method: "POST", url: "/v1/backfill-jobs", headers, payload: { ...payload, toUnix: fromUnix + 60 } });
    expect(duplicate.json<{ job: Job }>().job.id).toBe(job.id);
    const stored = await db.selectFrom("backfill_jobs").selectAll().where("id", "=", job.id).executeTakeFirstOrThrow();
    expect(stored.to_ts.getTime() / 1000).toBe(fromUnix + 60);
    expect(JSON.stringify(stored.requested_by)).not.toContain(headers["x-api-key"]);
    expect(await db.selectFrom("candles").select("instrument_id").where("instrument_id", "=", instrumentId).execute()).toEqual([]);
    const cancelUrl = `/v1/backfill-jobs/${job.id}/cancel`;
    const foreign = await app.inject({ method: "POST", url: cancelUrl, headers: { "x-api-key": "other-management-key" } });
    expect(foreign.json<{ job: Job }>().job.status).toBe("queued");
    await requestBackfill(db, { instrumentId, fromTs: new Date(fromUnix * 1000), toTs: new Date((fromUnix + 60) * 1000), requestedBy: "continuous" });
    const shared = await app.inject({ method: "POST", url: cancelUrl, headers });
    expect(shared.json<{ job: Job }>().job).toMatchObject({ status: "queued", requestedByCaller: false });
    await db.updateTable("backfill_jobs").set({ requested_by: stored.requested_by }).where("id", "=", job.id).execute();
    const cancelled = await app.inject({ method: "POST", url: cancelUrl, headers });
    expect(cancelled.json<{ job: Job }>().job).toMatchObject({ status: "cancel_requested", requestedByCaller: false });
    expect((await app.inject({ method: "POST", url: cancelUrl, headers })).statusCode).toBe(200);
  });

  it("paginates recent jobs and exposes checkpoint progress", async () => {
    const newer = await requestBackfill(db, { instrumentId, fromTs: new Date((fromUnix + 600) * 1000), toTs: new Date((fromUnix + 660) * 1000), requestedBy: "continuous" });
    await db.updateTable("backfill_jobs").set({ checkpoint_ts: new Date((fromUnix + 600) * 1000) }).where("id", "=", newer).execute();
    const page = await app.inject({ method: "GET", url: `/v1/backfill-jobs?instrumentId=${instrumentId}&limit=1`, headers });
    const result = page.json<{ jobs: Job[]; nextBeforeId: number }>();
    expect(result.jobs).toHaveLength(1);
    expect(result.jobs[0]).toMatchObject({ id: newer, checkpointUnix: fromUnix + 600 });
    expect(result.nextBeforeId).toBe(newer);
    const next = await app.inject({ method: "GET", url: `/v1/backfill-jobs?instrumentId=${instrumentId}&limit=1&beforeId=${newer}`, headers });
    expect(next.json<{ jobs: Job[] }>().jobs[0]!.id).toBeLessThan(newer);
  });

  it("persists enable/disable without fetching or deleting history", async () => {
    for (const enabled of [true, false]) {
      const response = await app.inject({ method: "PUT", url: `/v1/collection-targets/${instrumentId}`, headers, payload: { enabled } });
      expect(response.statusCode).toBe(200);
      expect(response.json()).toEqual({ instrumentId, enabled });
    }
    const response = await app.inject({ method: "GET", url: `/v1/collection-targets?afterInstrumentId=${instrumentId - 1}&limit=1`, headers });
    expect(response.json<{ targets: { instrumentId: number; enabled: boolean; lastCollectedUnix: null }[] }>().targets[0]).toMatchObject({ instrumentId, enabled: false, lastCollectedUnix: null });
  });

  it("bulk-updates collection targets for many instruments", async () => {
    const stamp = Date.now();
    const ids = [
      await ensureInstrument(db, { exchangeCode: "bybit", category: "linear", symbol: `TGT${stamp}A` }),
      await ensureInstrument(db, { exchangeCode: "bybit", category: "linear", symbol: `TGT${stamp}B` }),
    ];
    try {
      expect((await app.inject({ method: "PUT", url: "/v1/collection-targets", headers, payload: { instrumentIds: [ids[0]!, 2147483000], enabled: true } })).statusCode).toBe(404);
      const enabled = await app.inject({ method: "PUT", url: "/v1/collection-targets", headers, payload: { instrumentIds: ids, enabled: true } });
      expect(enabled.statusCode).toBe(200);
      expect(enabled.json()).toEqual({ updated: 2, enabled: true });
      let rows = await db.selectFrom("collection_targets").selectAll().where("instrument_id", "in", ids).orderBy("instrument_id", "asc").execute();
      expect(rows).toHaveLength(2);
      expect(rows.every((row) => row.enabled)).toBe(true);
      expect((await app.inject({ method: "PUT", url: "/v1/collection-targets", headers, payload: { instrumentIds: [ids[0]!], enabled: false } })).statusCode).toBe(200);
      rows = await db.selectFrom("collection_targets").selectAll().where("instrument_id", "in", ids).orderBy("instrument_id", "asc").execute();
      expect(rows[0]).toMatchObject({ instrument_id: ids[0], enabled: false });
      expect(rows[1]).toMatchObject({ instrument_id: ids[1], enabled: true });
    } finally {
      await db.deleteFrom("collection_targets").where("instrument_id", "in", ids).execute();
      await db.deleteFrom("instruments").where("id", "in", ids).execute();
    }
  });

  it("creates one job per instrument in a batch and exposes status counts", async () => {
    const stamp = Date.now();
    const ids = [
      await ensureInstrument(db, { exchangeCode: "bybit", category: "linear", symbol: `BATCH${stamp}A` }),
      await ensureInstrument(db, { exchangeCode: "bybit", category: "linear", symbol: `BATCH${stamp}B` }),
      await ensureInstrument(db, { exchangeCode: "bybit", category: "linear", symbol: `BATCH${stamp}C` }),
    ];
    const response = await app.inject({ method: "POST", url: "/v1/backfill-jobs/batch", headers, payload: { instrumentIds: ids, fromUnix, toUnix: fromUnix + 3600 } });
    expect(response.statusCode).toBe(202);
    const body = response.json<{ jobs: Job[]; created: number; declined: number; limit?: unknown }>();
    expect(body.created).toBe(3);
    expect(body.declined).toBe(0);
    expect(body.jobs).toHaveLength(3);
    expect(body.limit).toBeUndefined();
    const missing = await app.inject({ method: "POST", url: "/v1/backfill-jobs/batch", headers, payload: { instrumentIds: [ids[0]!, 2147483000], fromUnix, toUnix: fromUnix + 3600 } });
    expect(missing.statusCode).toBe(404);
    const counts = (await app.inject({ method: "GET", url: "/v1/backfill-jobs", headers })).json<{ counts: Record<string, number> }>().counts;
    expect(counts.queued).toBeGreaterThanOrEqual(3);
  });

  it("stops a batch at admission limits and reports created and declined counts", async () => {
    const before = (await app.inject({ method: "GET", url: "/v1/backfill-jobs", headers })).json<{ counts: { queued: number; running: number; cancel_requested: number } }>().counts;
    const activeBefore = before.queued + before.running + before.cancel_requested;
    const limited = await buildApp({
      db,
      apiKeys: [headers["x-api-key"]],
      openApiEnabled: false,
      backfillLimits: { maxPendingJobs: activeBefore + 2, maxWorkMinutes: 100_000_000, maxDatabaseBytes: 1_000_000_000_000, estimatedBytesPerWorkMinute: 1 },
    });
    try {
      const stamp = Date.now();
      const ids = [
        await ensureInstrument(db, { exchangeCode: "bybit", category: "linear", symbol: `LIM${stamp}A` }),
        await ensureInstrument(db, { exchangeCode: "bybit", category: "linear", symbol: `LIM${stamp}B` }),
        await ensureInstrument(db, { exchangeCode: "bybit", category: "linear", symbol: `LIM${stamp}C` }),
      ];
      const response = await limited.inject({ method: "POST", url: "/v1/backfill-jobs/batch", headers, payload: { instrumentIds: ids, fromUnix, toUnix: fromUnix + 3600 } });
      expect(response.statusCode).toBe(202);
      const body = response.json<{ created: number; declined: number; limit: { resource: string } }>();
      expect(body.created).toBe(2);
      expect(body.declined).toBe(1);
      expect(body.limit.resource).toBe("queue_jobs");
    } finally {
      await limited.close();
    }
  });

  it("creates a plan, defers beyond queue limits and auto-continues when the queue drains", async () => {
    const counts = (await app.inject({ method: "GET", url: "/v1/backfill-jobs", headers })).json<{ counts: { queued: number; running: number; cancel_requested: number } }>().counts;
    const activeBefore = counts.queued + counts.running + counts.cancel_requested;
    const limits = { maxPendingJobs: activeBefore + 2, maxWorkMinutes: 100_000_000, maxDatabaseBytes: 1_000_000_000_000, estimatedBytesPerWorkMinute: 1 };
    const limited = await buildApp({ db, apiKeys: [headers["x-api-key"]], openApiEnabled: false, backfillLimits: limits });
    try {
      expect((await limited.inject({ method: "POST", url: "/v1/backfill-plans", headers, payload: { instrumentIds: [planInstrumentIds[0]!, 2147483000], fromUnix, toUnix: fromUnix + 3600 } })).statusCode).toBe(404);
      expect((await limited.inject({ method: "POST", url: "/v1/backfill-plans", headers, payload: { instrumentIds: planInstrumentIds, fromUnix, toUnix: fromUnix } })).statusCode).toBe(400);
      const created = await limited.inject({ method: "POST", url: "/v1/backfill-plans", headers, payload: { instrumentIds: planInstrumentIds, fromUnix, toUnix: fromUnix + 3600 } });
      expect(created.statusCode).toBe(202);
      const plan = created.json<{ plan: { id: number; status: string; counts: { total: number; enqueued: number; pending: number; failed: number }; activeJobs: number } }>().plan;
      expect(plan.status).toBe("completed");
      expect(plan.counts).toMatchObject({ total: 2, enqueued: 2, pending: 0, failed: 0 });
      expect(plan.activeJobs).toBe(2);

      const thirdId = await ensureInstrument(db, { exchangeCode: "bybit", category: "linear", symbol: `PLNEXT${Date.now()}` });
      const bigger = { ...limits, maxPendingJobs: activeBefore + 2 };
      const extended = await buildApp({ db, apiKeys: [headers["x-api-key"]], openApiEnabled: false, backfillLimits: bigger });
      try {
        const grown = await extended.inject({ method: "POST", url: "/v1/backfill-plans", headers, payload: { instrumentIds: [thirdId], fromUnix, toUnix: fromUnix + 3600 } });
        expect(grown.json<{ plan: { counts: { enqueued: number; pending: number } } }>().plan.counts).toMatchObject({ enqueued: 0, pending: 1 });
        await db.updateTable("backfill_jobs").set({ status: "done", updated_at: new Date() })
          .where("instrument_id", "in", [...planInstrumentIds])
          .where("status", "in", ["queued", "running", "cancel_requested"])
          .execute();
        const topUp = await topUpBackfillPlans(db, bigger);
        expect(topUp.enqueued).toBeGreaterThanOrEqual(1);
        const listed = (await extended.inject({ method: "GET", url: "/v1/backfill-plans", headers })).json<{ plans: { id: number; status: string; counts: { enqueued: number; pending: number } }[] }>().plans;
        const first = listed.find((row) => row.id === plan.id);
        const second = listed.find((row) => row.id !== plan.id);
        expect(first?.status).toBe("completed");
        expect(second?.status).toBe("completed");
        expect(second?.counts.pending).toBe(0);
        await db.deleteFrom("backfill_jobs").where("instrument_id", "=", thirdId).execute();
        await db.deleteFrom("instruments").where("id", "=", thirdId).execute();
      } finally {
        await extended.close();
      }
    } finally {
      await limited.close();
      await db.deleteFrom("backfill_plan_items").execute();
      await db.deleteFrom("backfill_plans").execute();
      await db.deleteFrom("backfill_jobs").where("instrument_id", "in", planInstrumentIds).execute();
    }
  });

  it("cancels a plan: stops enqueueing and requests cancellation of its active jobs", async () => {
    const stamp = Date.now();
    const ids = [
      await ensureInstrument(db, { exchangeCode: "bybit", category: "linear", symbol: `PLC${stamp}A` }),
      await ensureInstrument(db, { exchangeCode: "bybit", category: "linear", symbol: `PLC${stamp}B` }),
    ];
    const counts = (await app.inject({ method: "GET", url: "/v1/backfill-jobs", headers })).json<{ counts: { queued: number; running: number; cancel_requested: number } }>().counts;
    const activeBefore = counts.queued + counts.running + counts.cancel_requested;
    const limited = await buildApp({ db, apiKeys: [headers["x-api-key"]], openApiEnabled: false, backfillLimits: { maxPendingJobs: activeBefore + 1, maxWorkMinutes: 100_000_000, maxDatabaseBytes: 1_000_000_000_000, estimatedBytesPerWorkMinute: 1 } });
    try {
      const created = await limited.inject({ method: "POST", url: "/v1/backfill-plans", headers, payload: { instrumentIds: ids, fromUnix, toUnix: fromUnix + 3600 } });
      expect(created.json<{ plan: { counts: { enqueued: number; pending: number } } }>().plan.counts).toMatchObject({ enqueued: 1, pending: 1 });
      const planId = created.json<{ plan: { id: number } }>().plan.id;
      expect((await limited.inject({ method: "POST", url: "/v1/backfill-plans/2147483647/cancel", headers })).statusCode).toBe(404);
      const cancelled = await limited.inject({ method: "POST", url: `/v1/backfill-plans/${planId}/cancel`, headers });
      expect(cancelled.statusCode).toBe(200);
      const after = cancelled.json<{ plan: { status: string; counts: { enqueued: number; pending: number; cancelled: number }; activeJobs: number } }>().plan;
      expect(after.status).toBe("cancelled");
      expect(after.counts).toMatchObject({ enqueued: 1, pending: 0, cancelled: 1 });
      expect(after.activeJobs).toBe(1);
      const jobs = await db.selectFrom("backfill_jobs").selectAll().where("instrument_id", "in", ids).execute();
      expect(jobs.filter((job) => job.status === "cancel_requested")).toHaveLength(1);
    } finally {
      await limited.close();
      await db.deleteFrom("backfill_plan_items").execute();
      await db.deleteFrom("backfill_plans").execute();
      await db.deleteFrom("backfill_jobs").where("instrument_id", "in", ids).execute();
      await db.deleteFrom("instruments").where("id", "in", ids).execute();
    }
  });

  it("marks items with oversized work failed and completes the plan without jobs", async () => {
    const stamp = Date.now();
    const ids = [
      await ensureInstrument(db, { exchangeCode: "bybit", category: "linear", symbol: `PLW${stamp}A` }),
      await ensureInstrument(db, { exchangeCode: "bybit", category: "linear", symbol: `PLW${stamp}B` }),
    ];
    const limited = await buildApp({ db, apiKeys: [headers["x-api-key"]], openApiEnabled: false, backfillLimits: { maxPendingJobs: 1000, maxWorkMinutes: 60, maxDatabaseBytes: 1_000_000_000_000, estimatedBytesPerWorkMinute: 1 } });
    try {
      const created = await limited.inject({ method: "POST", url: "/v1/backfill-plans", headers, payload: { instrumentIds: ids, fromUnix, toUnix: fromUnix + 7200 } });
      expect(created.statusCode).toBe(202);
      const plan = created.json<{ plan: { id: number; status: string; counts: { failed: number; pending: number; enqueued: number } } }>().plan;
      expect(plan.status).toBe("completed");
      expect(plan.counts).toMatchObject({ failed: 2, pending: 0, enqueued: 0 });
      expect(await db.selectFrom("backfill_jobs").select("id").where("instrument_id", "in", ids).execute()).toEqual([]);
      const items = await db.selectFrom("backfill_plan_items").selectAll().where("plan_id", "=", plan.id).execute();
      expect(items.every((item) => item.status === "failed" && item.error?.startsWith("work_minutes_exceeded"))).toBe(true);
    } finally {
      await limited.close();
      await db.deleteFrom("backfill_plan_items").execute();
      await db.deleteFrom("backfill_plans").execute();
      await db.deleteFrom("instruments").where("id", "in", ids).execute();
    }
  });

  it("imports the current exchange listing into the instrument catalog on demand", async () => {
    const symbol = `LIST${Date.now()}USDT`;
    const withListing = await buildApp({ db, apiKeys: [headers["x-api-key"]], openApiEnabled: false, exchangeListing: () => Promise.resolve(["BTCUSDT", symbol]) });
    try {
      const response = await withListing.inject({ method: "GET", url: "/v1/instruments/listing", headers });
      expect(response.statusCode).toBe(200);
      const body = response.json<{ instruments: { exchange: string; category: string; symbol: string }[] }>();
      expect(body.instruments.map((entry) => entry.symbol)).toEqual(["BTCUSDT", symbol]);
      expect(body.instruments.every((entry) => entry.exchange === "bybit" && entry.category === "linear")).toBe(true);
      const persisted = await db.selectFrom("instruments").select("symbol").where("symbol", "=", symbol).executeTakeFirst();
      expect(persisted?.symbol).toBe(symbol);
    } finally {
      await withListing.close();
    }
    expect((await app.inject({ method: "GET", url: "/v1/instruments/listing", headers })).statusCode).toBe(503);
  });
});
