import { createHash } from "node:crypto";
import { Type } from "@sinclair/typebox";
import type { FastifyInstance, FastifyRequest } from "fastify";
import { sql, type Selectable } from "kysely";
import type { BackfillJobsTable } from "../db/schema.js";
import { BackfillAdmissionError, cancelBackfillRequest, requestBackfill } from "../ingest/backfill.js";
import { createBackfillPlan, cancelBackfillPlan, getBackfillPlan, listBackfillPlans, type BackfillPlan } from "../ingest/plans.js";
import { setCollectionTarget } from "../ingest/continuous.js";
import { ensureInstruments } from "../storage/instruments.js";
import type { V1RouteOptions } from "./v1.js";

const Id = Type.Integer({ minimum: 1, maximum: 2147483647 });
const Unix = Type.Integer({ minimum: 0, maximum: 253402300740, multipleOf: 60 });
const ErrorResponse = Type.Object({ error: Type.String() });
const ResourceLimitResponse = Type.Object({
  error: Type.String(),
  resource: Type.Union([Type.Literal("queue_jobs"), Type.Literal("work_minutes"), Type.Literal("database_bytes")]),
  requested: Type.Number(),
  limit: Type.Number(),
});
const Job = Type.Object({
  id: Id,
  instrumentId: Id,
  fromUnix: Type.Integer(),
  toUnix: Type.Integer(),
  status: Type.Union(["queued", "running", "done", "cancel_requested", "cancelled", "failed"].map((status) => Type.Literal(status))),
  checkpointUnix: Type.Union([Type.Integer(), Type.Null()]),
  error: Type.Union([Type.String(), Type.Null()]),
  createdAt: Type.String(),
  updatedAt: Type.String(),
  requestedByCaller: Type.Boolean(),
});
const JobCounts = Type.Object({
  queued: Type.Integer(),
  running: Type.Integer(),
  done: Type.Integer(),
  cancel_requested: Type.Integer(),
  cancelled: Type.Integer(),
  failed: Type.Integer(),
});
const Target = Type.Object({
  instrumentId: Id,
  enabled: Type.Boolean(),
  lastCollectedUnix: Type.Union([Type.Integer(), Type.Null()]),
  updatedAt: Type.String(),
});

function caller(request: FastifyRequest): string {
  // Stable per credential, without persisting the credential itself.
  return `api:${createHash("sha256").update(String(request.headers["x-api-key"])).digest("hex")}`;
}

function jobDto(row: Selectable<BackfillJobsTable>, requestedBy: string) {
  return {
    id: row.id,
    instrumentId: row.instrument_id,
    fromUnix: row.from_ts.getTime() / 1000,
    toUnix: row.to_ts.getTime() / 1000 + 60,
    status: row.status,
    checkpointUnix: row.checkpoint_ts ? row.checkpoint_ts.getTime() / 1000 : null,
    error: row.error?.slice(0, 1000) ?? null,
    createdAt: row.created_at.toISOString(),
    updatedAt: row.updated_at.toISOString(),
    requestedByCaller: row.requested_by.includes(requestedBy),
  };
}

const Plan = Type.Object({
  id: Id,
  fromUnix: Type.Integer(),
  toUnix: Type.Integer(),
  status: Type.Union([Type.Literal("active"), Type.Literal("completed"), Type.Literal("cancelled")]),
  counts: Type.Object({ total: Type.Integer(), pending: Type.Integer(), enqueued: Type.Integer(), failed: Type.Integer(), cancelled: Type.Integer() }),
  activeJobs: Type.Integer(),
  createdAt: Type.String(),
  updatedAt: Type.String(),
});

function planDto(plan: BackfillPlan) {
  return {
    id: plan.id,
    fromUnix: plan.fromTs.getTime() / 1000,
    toUnix: plan.toTs.getTime() / 1000 + 60,
    status: plan.status as "active" | "completed" | "cancelled",
    counts: plan.counts,
    activeJobs: plan.activeJobs,
    createdAt: plan.createdAt.toISOString(),
    updatedAt: plan.updatedAt.toISOString(),
  };
}

export function registerManagementRoutes(app: FastifyInstance, { db, requireApiKey, backfillLimits, exchangeListing }: V1RouteOptions): void {
  app.get<{ Querystring: { limit?: number; beforeId?: number; instrumentId?: number } }>("/v1/backfill-jobs", {
    onRequest: [requireApiKey],
    schema: {
      querystring: Type.Object({ limit: Type.Optional(Type.Integer({ minimum: 1, maximum: 200 })), beforeId: Type.Optional(Id), instrumentId: Type.Optional(Id) }),
      response: { 200: Type.Object({ jobs: Type.Array(Job), nextBeforeId: Type.Union([Id, Type.Null()]), counts: JobCounts }), 401: ErrorResponse },
    },
  }, async (request) => {
    const limit = request.query.limit ?? 50;
    let query = db.selectFrom("backfill_jobs").selectAll().orderBy("id", "desc").limit(limit + 1);
    if (request.query.beforeId !== undefined) query = query.where("id", "<", request.query.beforeId);
    if (request.query.instrumentId !== undefined) query = query.where("instrument_id", "=", request.query.instrumentId);
    const rows = await query.execute();
    const page = rows.slice(0, limit);
    const countRows = await db.selectFrom("backfill_jobs").select(["status", sql<number>`count(*)::int`.as("count")]).groupBy("status").execute();
    const counts = { queued: 0, running: 0, done: 0, cancel_requested: 0, cancelled: 0, failed: 0 };
    for (const row of countRows) {
      if (row.status in counts) counts[row.status] = Number(row.count);
    }
    return { jobs: page.map((row) => jobDto(row, caller(request))), nextBeforeId: rows.length > limit ? page.at(-1)!.id : null, counts };
  });

  app.get("/v1/instruments/listing", {
    onRequest: [requireApiKey],
    schema: {
      response: {
        200: Type.Object({ instruments: Type.Array(Type.Object({ id: Id, exchange: Type.String(), category: Type.String(), symbol: Type.String(), status: Type.String() })) }),
        401: ErrorResponse,
        502: ErrorResponse,
        503: ErrorResponse,
      },
    },
  }, async (request, reply) => {
    if (!exchangeListing) return reply.code(503).send({ error: "exchange_listing_unavailable" });
    let symbols: string[];
    try {
      symbols = await exchangeListing();
    } catch (error) {
      request.log.error(error, "exchange listing fetch failed");
      return reply.code(502).send({ error: "exchange_listing_failed" });
    }
    const rows = await ensureInstruments(db, { exchangeCode: "bybit", category: "linear" }, symbols);
    return { instruments: rows.map((row) => ({ id: row.id, exchange: "bybit", category: "linear", symbol: row.symbol, status: row.status })) };
  });

  app.post<{ Body: { instrumentId: number; fromUnix: number; toUnix: number } }>("/v1/backfill-jobs", {
    onRequest: [requireApiKey],
    schema: {
      body: Type.Object({ instrumentId: Id, fromUnix: Unix, toUnix: Unix }, { additionalProperties: false }),
      response: { 202: Type.Object({ job: Job }), 400: ErrorResponse, 401: ErrorResponse, 404: ErrorResponse, 429: ResourceLimitResponse },
    },
  }, async (request, reply) => {
    const { instrumentId, fromUnix, toUnix } = request.body;
    if (toUnix <= fromUnix || toUnix - fromUnix > 5 * 366 * 86400 || toUnix > Math.floor(Date.now() / 60000) * 60) {
      return reply.code(400).send({ error: "invalid_range: require closed minutes in [fromUnix,toUnix), maximum 1830 days" });
    }
    const instrument = await db.selectFrom("instruments").select("id").where("id", "=", instrumentId).executeTakeFirst();
    if (!instrument) return reply.code(404).send({ error: "instrument_not_found" });
    const requestedBy = caller(request);
    let id: number;
    try {
      id = await requestBackfill(db, { instrumentId, fromTs: new Date(fromUnix * 1000), toTs: new Date((toUnix - 60) * 1000), requestedBy }, backfillLimits);
    } catch (error) {
      if (error instanceof BackfillAdmissionError) {
        return reply.code(429).send({
          error: `resource_limit_exceeded:${error.resource}:${error.requested}:${error.limit}`,
          resource: error.resource,
          requested: error.requested,
          limit: error.limit,
        });
      }
      throw error;
    }
    const row = await db.selectFrom("backfill_jobs").selectAll().where("id", "=", id).executeTakeFirstOrThrow();
    return reply.code(202).send({ job: jobDto(row, requestedBy) });
  });

  app.post<{ Body: { instrumentIds: number[]; fromUnix: number; toUnix: number } }>("/v1/backfill-jobs/batch", {
    onRequest: [requireApiKey],
    schema: {
      body: Type.Object({ instrumentIds: Type.Array(Id, { minItems: 1, maxItems: 1000, uniqueItems: true }), fromUnix: Unix, toUnix: Unix }, { additionalProperties: false }),
      response: {
        202: Type.Object({ jobs: Type.Array(Job), created: Type.Integer(), declined: Type.Integer(), limit: Type.Optional(ResourceLimitResponse) }),
        400: ErrorResponse,
        401: ErrorResponse,
        404: ErrorResponse,
      },
    },
  }, async (request, reply) => {
    const { instrumentIds, fromUnix, toUnix } = request.body;
    if (toUnix <= fromUnix || toUnix - fromUnix > 5 * 366 * 86400 || toUnix > Math.floor(Date.now() / 60000) * 60) {
      return reply.code(400).send({ error: "invalid_range: require closed minutes in [fromUnix,toUnix), maximum 1830 days" });
    }
    const found = await db.selectFrom("instruments").select("id").where("id", "in", instrumentIds).execute();
    if (found.length !== instrumentIds.length) return reply.code(404).send({ error: "instrument_not_found" });
    const requestedBy = caller(request);
    const jobs: ReturnType<typeof jobDto>[] = [];
    let admission: BackfillAdmissionError | null = null;
    for (const instrumentId of instrumentIds) {
      try {
        const id = await requestBackfill(db, { instrumentId, fromTs: new Date(fromUnix * 1000), toTs: new Date((toUnix - 60) * 1000), requestedBy }, backfillLimits);
        const row = await db.selectFrom("backfill_jobs").selectAll().where("id", "=", id).executeTakeFirstOrThrow();
        jobs.push(jobDto(row, requestedBy));
      } catch (error) {
        if (error instanceof BackfillAdmissionError) {
          admission = error;
          break;
        }
        throw error;
      }
    }
    return reply.code(202).send({
      jobs,
      created: jobs.length,
      declined: instrumentIds.length - jobs.length,
      ...(admission ? { limit: { error: `resource_limit_exceeded:${admission.resource}:${admission.requested}:${admission.limit}`, resource: admission.resource, requested: admission.requested, limit: admission.limit } } : {}),
    });
  });

  app.get("/v1/backfill-plans", {
    onRequest: [requireApiKey],
    schema: { response: { 200: Type.Object({ plans: Type.Array(Plan) }), 401: ErrorResponse } },
  }, async () => {
    const plans = await listBackfillPlans(db);
    return { plans: plans.map(planDto) };
  });

  app.post<{ Body: { instrumentIds: number[]; fromUnix: number; toUnix: number } }>("/v1/backfill-plans", {
    onRequest: [requireApiKey],
    schema: {
      body: Type.Object({ instrumentIds: Type.Array(Id, { minItems: 1, maxItems: 1000, uniqueItems: true }), fromUnix: Unix, toUnix: Unix }, { additionalProperties: false }),
      response: { 202: Type.Object({ plan: Plan }), 400: ErrorResponse, 401: ErrorResponse, 404: ErrorResponse },
    },
  }, async (request, reply) => {
    const { instrumentIds, fromUnix, toUnix } = request.body;
    if (toUnix <= fromUnix || toUnix - fromUnix > 5 * 366 * 86400 || toUnix > Math.floor(Date.now() / 60000) * 60) {
      return reply.code(400).send({ error: "invalid_range: require closed minutes in [fromUnix,toUnix), maximum 1830 days" });
    }
    const found = await db.selectFrom("instruments").select("id").where("id", "in", instrumentIds).execute();
    if (found.length !== instrumentIds.length) return reply.code(404).send({ error: "instrument_not_found" });
    const id = await createBackfillPlan(
      db,
      { instrumentIds, fromTs: new Date(fromUnix * 1000), toTs: new Date((toUnix - 60) * 1000), requestedBy: caller(request) },
      backfillLimits,
    );
    const plan = await getBackfillPlan(db, id);
    return reply.code(202).send({ plan: planDto(plan!) });
  });

  app.post<{ Params: { id: number } }>("/v1/backfill-plans/:id/cancel", {
    onRequest: [requireApiKey],
    schema: { params: Type.Object({ id: Id }), response: { 200: Type.Object({ plan: Plan }), 401: ErrorResponse, 404: ErrorResponse } },
  }, async (request, reply) => {
    const cancelled = await cancelBackfillPlan(db, request.params.id);
    if (!cancelled) return reply.code(404).send({ error: "plan_not_found" });
    const plan = await getBackfillPlan(db, request.params.id);
    return { plan: planDto(plan!) };
  });

  app.post<{ Params: { id: number } }>("/v1/backfill-jobs/:id/cancel", {
    onRequest: [requireApiKey],
    schema: { params: Type.Object({ id: Id }), response: { 200: Type.Object({ job: Job }), 401: ErrorResponse, 404: ErrorResponse } },
  }, async (request, reply) => {
    const row = await db.selectFrom("backfill_jobs").selectAll().where("id", "=", request.params.id).executeTakeFirst();
    if (!row) return reply.code(404).send({ error: "job_not_found" });
    const requestedBy = caller(request);
    if (row.requested_by.includes(requestedBy)) await cancelBackfillRequest(db, row.id, requestedBy);
    const current = await db.selectFrom("backfill_jobs").selectAll().where("id", "=", row.id).executeTakeFirstOrThrow();
    return { job: jobDto(current, requestedBy) };
  });

  app.get<{ Querystring: { limit?: number; afterInstrumentId?: number } }>("/v1/collection-targets", {
    onRequest: [requireApiKey],
    schema: {
      querystring: Type.Object({ limit: Type.Optional(Type.Integer({ minimum: 1, maximum: 1000 })), afterInstrumentId: Type.Optional(Id) }),
      response: { 200: Type.Object({ targets: Type.Array(Target), nextAfterInstrumentId: Type.Union([Id, Type.Null()]) }), 401: ErrorResponse },
    },
  }, async (request) => {
    const limit = request.query.limit ?? 500;
    let query = db.selectFrom("collection_targets").selectAll().orderBy("instrument_id", "asc").limit(limit + 1);
    if (request.query.afterInstrumentId !== undefined) query = query.where("instrument_id", ">", request.query.afterInstrumentId);
    const rows = await query.execute();
    const page = rows.slice(0, limit);
    return {
      targets: page.map((row) => ({ instrumentId: row.instrument_id, enabled: row.enabled, lastCollectedUnix: row.last_collected_ts ? row.last_collected_ts.getTime() / 1000 : null, updatedAt: row.updated_at.toISOString() })),
      nextAfterInstrumentId: rows.length > limit ? page.at(-1)!.instrument_id : null,
    };
  });

  app.put<{ Params: { instrumentId: number }; Body: { enabled: boolean } }>("/v1/collection-targets/:instrumentId", {
    onRequest: [requireApiKey],
    schema: { params: Type.Object({ instrumentId: Id }), body: Type.Object({ enabled: Type.Boolean() }, { additionalProperties: false }), response: { 200: Type.Object({ instrumentId: Id, enabled: Type.Boolean() }), 401: ErrorResponse, 404: ErrorResponse } },
  }, async (request, reply) => {
    const { instrumentId } = request.params;
    const instrument = await db.selectFrom("instruments").select("id").where("id", "=", instrumentId).executeTakeFirst();
    if (!instrument) return reply.code(404).send({ error: "instrument_not_found" });
    await setCollectionTarget(db, instrumentId, request.body.enabled);
    return { instrumentId, enabled: request.body.enabled };
  });
}
