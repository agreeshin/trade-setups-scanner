import { createHash } from "node:crypto";
import { Type } from "@sinclair/typebox";
import type { FastifyInstance, FastifyRequest } from "fastify";
import type { Selectable } from "kysely";
import type { BackfillJobsTable } from "../db/schema.js";
import { cancelBackfillRequest, requestBackfill } from "../ingest/backfill.js";
import { setCollectionTarget } from "../ingest/continuous.js";
import type { V1RouteOptions } from "./v1.js";

const Id = Type.Integer({ minimum: 1, maximum: 2147483647 });
const Unix = Type.Integer({ minimum: 0, maximum: 253402300740, multipleOf: 60 });
const ErrorResponse = Type.Object({ error: Type.String() });
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

export function registerManagementRoutes(app: FastifyInstance, { db, requireApiKey }: V1RouteOptions): void {
  app.get<{ Querystring: { limit?: number; beforeId?: number; instrumentId?: number } }>("/v1/backfill-jobs", {
    onRequest: [requireApiKey],
    schema: {
      querystring: Type.Object({ limit: Type.Optional(Type.Integer({ minimum: 1, maximum: 200 })), beforeId: Type.Optional(Id), instrumentId: Type.Optional(Id) }),
      response: { 200: Type.Object({ jobs: Type.Array(Job), nextBeforeId: Type.Union([Id, Type.Null()]) }), 401: ErrorResponse },
    },
  }, async (request) => {
    const limit = request.query.limit ?? 50;
    let query = db.selectFrom("backfill_jobs").selectAll().orderBy("id", "desc").limit(limit + 1);
    if (request.query.beforeId !== undefined) query = query.where("id", "<", request.query.beforeId);
    if (request.query.instrumentId !== undefined) query = query.where("instrument_id", "=", request.query.instrumentId);
    const rows = await query.execute();
    const page = rows.slice(0, limit);
    return { jobs: page.map((row) => jobDto(row, caller(request))), nextBeforeId: rows.length > limit ? page.at(-1)!.id : null };
  });

  app.post<{ Body: { instrumentId: number; fromUnix: number; toUnix: number } }>("/v1/backfill-jobs", {
    onRequest: [requireApiKey],
    schema: {
      body: Type.Object({ instrumentId: Id, fromUnix: Unix, toUnix: Unix }, { additionalProperties: false }),
      response: { 202: Type.Object({ job: Job }), 400: ErrorResponse, 401: ErrorResponse, 404: ErrorResponse },
    },
  }, async (request, reply) => {
    const { instrumentId, fromUnix, toUnix } = request.body;
    if (toUnix <= fromUnix || toUnix - fromUnix > 5 * 366 * 86400 || toUnix > Math.floor(Date.now() / 60000) * 60) {
      return reply.code(400).send({ error: "invalid_range: require closed minutes in [fromUnix,toUnix), maximum 1830 days" });
    }
    const instrument = await db.selectFrom("instruments").select("id").where("id", "=", instrumentId).executeTakeFirst();
    if (!instrument) return reply.code(404).send({ error: "instrument_not_found" });
    const requestedBy = caller(request);
    const id = await requestBackfill(db, { instrumentId, fromTs: new Date(fromUnix * 1000), toTs: new Date((toUnix - 60) * 1000), requestedBy });
    const row = await db.selectFrom("backfill_jobs").selectAll().where("id", "=", id).executeTakeFirstOrThrow();
    return reply.code(202).send({ job: jobDto(row, requestedBy) });
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
