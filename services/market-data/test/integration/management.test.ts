import { afterAll, beforeAll, describe, expect, it } from "vitest";
import type { FastifyInstance } from "fastify";
import type { Kysely } from "kysely";
import { buildApp } from "../../src/app.js";
import { createDb } from "../../src/db/db.js";
import { createMigrator } from "../../src/db/migrator.js";
import type { MarketDataDB } from "../../src/db/schema.js";
import { requestBackfill } from "../../src/ingest/backfill.js";
import { ensureInstrument } from "../../src/storage/instruments.js";

const url = process.env.TEST_DATABASE_URL;
const headers = { "x-api-key": "management-test-key" };
const fromUnix = 1704067200;
interface Job { id: number; instrumentId: number; fromUnix: number; toUnix: number; status: string; requestedByCaller: boolean; checkpointUnix: number | null }

describe.skipIf(!url)("history management API", () => {
  let db: Kysely<MarketDataDB>;
  let app: FastifyInstance;
  let instrumentId: number;

  beforeAll(async () => {
    if (!/test/i.test(new URL(url!).pathname)) throw new Error("Management tests require an isolated test database");
    db = createDb(url!);
    const migration = await createMigrator(db).migrateToLatest();
    if (migration.error) throw new Error("Management test migration failed", { cause: migration.error });
    instrumentId = await ensureInstrument(db, { exchangeCode: "bybit", category: "linear", symbol: `MGMT${Date.now()}USDT` });
    app = await buildApp({ db, apiKeys: [headers["x-api-key"], "other-management-key"], openApiEnabled: true });
  });
  afterAll(async () => {
    await app?.close();
    if (db) {
      if (instrumentId) {
        await db.deleteFrom("collection_targets").where("instrument_id", "=", instrumentId).execute();
        await db.deleteFrom("backfill_jobs").where("instrument_id", "=", instrumentId).execute();
        await db.deleteFrom("instruments").where("id", "=", instrumentId).execute();
      }
      await db.destroy();
    }
  });

  it("protects every management route before validation", async () => {
    for (const method of ["GET", "POST", "PUT"] as const) {
      const paths = method === "GET" ? ["/v1/backfill-jobs", "/v1/collection-targets"] : method === "POST" ? ["/v1/backfill-jobs", "/v1/backfill-jobs/1/cancel"] : ["/v1/collection-targets/1"];
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
});
