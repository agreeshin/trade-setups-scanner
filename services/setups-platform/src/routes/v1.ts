import type { FastifyInstance, FastifyReply, FastifyRequest } from "fastify";
import { Readable } from "node:stream";
import { Type } from "@sinclair/typebox";
import { sql, type Kysely } from "kysely";
import type { SignalsDB } from "../db/schema.js";
import type { SpikeSetup } from "../detector/price-volume-spike.js";
import { SPIKE_ALGORITHM_ID, SPIKE_ALGORITHM_VERSION } from "../detector/price-volume-spike.js";
import { SpikeParamsSchema } from "../algorithms.js";
import { registerControls } from "./controls.js";
import { ApiCandleSource } from "../replay/source.js";
import { replayRun } from "../replay/replay-runner.js";
import { cancelReplayRun, enqueueReplayRuns, replayRunDto } from "../replay/jobs.js";
import { saveQualityPage, type QualityInterval } from "../replay/quality.js";
import { prepareReplayRequest, replayPreflight, type ReplayRequest } from "../replay/preflight.js";
import {
  cacheClearAll,
  cacheClearByInstrument,
  cacheLookup,
  cacheStats,
  cacheStore,
  cacheGeneration,
  lockReplayCache,
  type CacheQuery,
} from "../replay/cache.js";
import { fetchCatalog } from "../platform-catalog.js";
import {
  getNotificationMaxAgeSeconds,
  setNotificationMaxAgeSeconds,
  getSetting,
  setSetting,
} from "../settings.js";

export type ApiKeyGuard = (
  request: FastifyRequest,
  reply: FastifyReply,
) => Promise<void>;

const ErrorEnvelope = Type.Object({ error: Type.String() });

const InstrumentInput = Type.Object({
  exchange: Type.String(),
  category: Type.String(),
  symbol: Type.String(),
});

const ReplayRunRequest = Type.Object({
  instruments: Type.Array(InstrumentInput, { minItems: 1, maxItems: 100 }),
  fromUnix: Type.Integer({ minimum: 0, maximum: 253_402_300_740 }),
  toUnix: Type.Integer({ minimum: 0, maximum: 253_402_300_740 }),
  algorithmId: Type.Optional(Type.String()),
  algorithmVersion: Type.Optional(Type.String()),
  params: Type.Optional(Type.Partial(SpikeParamsSchema)),
  async: Type.Optional(Type.Boolean()),
});

const SetupDto = Type.Object({
  exchange: Type.String(),
  category: Type.String(),
  symbol: Type.String(),
  algorithmId: Type.String(),
  algorithmVersion: Type.Optional(Type.String()),
  params: Type.Optional(Type.Record(Type.String(), Type.Unknown())),
  details: Type.Optional(Type.Union([Type.Null(), Type.Object({
    growthPct: Type.Union([Type.String(), Type.Null()]),
    volumeLongRatio: Type.Union([Type.String(), Type.Null()]),
    volumeShortRatio: Type.Union([Type.String(), Type.Null()]),
    overLong: Type.Boolean(),
    overShort: Type.Boolean(),
    missingMinutes: Type.Integer(),
  })])),
  openTimeUnix: Type.Integer(),
  closeTimeUnix: Type.Integer(),
  closePrice: Type.String(),
});

const RunDto = Type.Object({
  quality: Type.Optional(Type.Union([Type.Record(Type.String(), Type.Unknown()), Type.Null()])),
  qualitySourceRunId: Type.Optional(Type.Union([Type.Integer(), Type.Null()])),
  qualityComputedAt: Type.Optional(Type.Union([Type.String(), Type.Null()])),
  id: Type.Integer(),
  exchange: Type.String(),
  category: Type.String(),
  symbol: Type.String(),
  status: Type.String(),
  cacheHit: Type.Boolean(),
  setupCount: Type.Integer(),
  warmupMinutes: Type.Union([Type.Integer(), Type.Null()]),
  processedMinutes: Type.Union([Type.Integer(), Type.Null()]),
  error: Type.Union([Type.String(), Type.Null()]),
  algorithmId: Type.Optional(Type.String()),
  algorithmVersion: Type.Optional(Type.String()),
  fromUnix: Type.Optional(Type.Integer()),
  toUnix: Type.Optional(Type.Integer()),
  createdAt: Type.Optional(Type.String()),
  params: Type.Optional(Type.Record(Type.String(), Type.Unknown())),
});

const toJsonArray = <T>(value: T[]): T[] => JSON.stringify(value) as unknown as T[];

function replayResultPage(db: Kysely<SignalsDB>, id: number, offset: number, limit: number) {
  // PostgreSQL jsonpath indices are int32; larger offsets are necessarily past the array end.
  const path = `$[${offset} to ${Math.min(offset + limit - 1, 2_147_483_647)}]`;
  const page = offset > 2_147_483_647 ? sql<SpikeSetup[]>`'[]'::jsonb`
    : sql<SpikeSetup[]>`jsonb_path_query_array(coalesce(setups, '[]'::jsonb), ${path}::jsonpath)`;
  return db.selectFrom("replay_runs")
    .select(["id", "exchange_code", "category", "symbol", "status", "cache_hit", "warmup_minutes",
      "processed_minutes", "error", "algorithm_id", "algorithm_version", "from_ts", "to_ts", "created_at", "params", "quality", "quality_source_run_id", "quality_computed_at"])
    .select(sql<number>`coalesce(jsonb_array_length(setups), 0)`.as("setup_count"))
    .select(page.as("setups"))
    .where("id", "=", id).executeTakeFirst();
}

function csvCell(value: string | number | boolean | null | undefined): string {
  let text = value === null || value === undefined ? "" : String(value);
  // Quoting alone does not stop spreadsheet formula execution.
  if (/^[\s\p{Cc}]*[=+@-]/u.test(text) || /^[\t\r\n]/u.test(text)) text = `'${text}`;
  return `"${text.replaceAll('"', '""')}"`;
}

export interface PlatformV1Options {
  db: Kysely<SignalsDB>;
  requireApiKey: ApiKeyGuard;
  marketData: { url: string; apiKey: string };
}

export function registerPlatformV1Routes(
  app: FastifyInstance,
  options: PlatformV1Options,
): void {
  const { db, requireApiKey, marketData } = options;
  registerControls(app, options);

  app.post("/v1/replay-preflight", { preHandler: [requireApiKey], schema: { body: ReplayRunRequest } }, async (request, reply) => {
    const controller = new AbortController();
    const close = () => controller.abort();
    reply.raw.once("close", close);
    if (reply.raw.destroyed) controller.abort();
    try {
      const prepared = await prepareReplayRequest(db, request.body as ReplayRequest);
      if (controller.signal.aborted) return reply;
      if ("error" in prepared) return reply.code(400).send({ error: prepared.error });
      return await replayPreflight(db, prepared.request, marketData, controller.signal);
    } finally {
      reply.raw.off("close", close);
      controller.abort();
    }
  });

  app.post(
    "/v1/replay-runs",
    {
      preHandler: [requireApiKey],
      schema: {
        body: ReplayRunRequest,
        response: { 200: Type.Object({ runs: Type.Array(RunDto) }), 202: Type.Object({ runs: Type.Array(RunDto) }), 400: ErrorEnvelope, 401: ErrorEnvelope, 429: ErrorEnvelope },
      },
    },
    async (request, reply) => {
      const prepared = await prepareReplayRequest(db, request.body as ReplayRequest);
      if ("error" in prepared) return reply.code(400).send({ error: prepared.error });
      const body = prepared.request;
      const params = body.params;

      if (body.async) {
        const queued = await enqueueReplayRuns(db, { ...body, params });
        if (!queued) return reply.code(429).send({ error: "replay_queue_full_max_100" });
        return reply.code(202).send({ runs: queued.map(replayRunDto) });
      }
      const generation = await cacheGeneration(db);

      const catalog = await fetchCatalog(marketData.url, marketData.apiKey);
      const byIdentity = new Map(
        catalog.map((item) => [`${item.exchange}/${item.category}/${item.symbol}`, item]),
      );

      const runs: Record<string, unknown>[] = [];
      for (const instrument of body.instruments) {
        const catalogEntry = byIdentity.get(
          `${instrument.exchange}/${instrument.category}/${instrument.symbol}`,
        );
        if (catalogEntry === undefined) {
          const failed = await db
            .insertInto("replay_runs")
            .values({
              exchange_code: instrument.exchange,
              category: instrument.category,
              symbol: instrument.symbol,
              from_ts: new Date(body.fromUnix * 1000),
              to_ts: new Date(body.toUnix * 1000),
              algorithm_id: SPIKE_ALGORITHM_ID,
              algorithm_version: SPIKE_ALGORITHM_VERSION,
              params,
              status: "failed",
              cache_hit: false,
              error: "instrument not found in market data catalog",
            })
            .returningAll()
            .executeTakeFirstOrThrow();
          runs.push({
            id: failed.id,
            exchange: failed.exchange_code,
            category: failed.category,
            symbol: failed.symbol,
            status: failed.status,
            cacheHit: failed.cache_hit,
            setupCount: 0,
            warmupMinutes: null,
            processedMinutes: null,
            error: failed.error,
          });
          continue;
        }

        const cacheQuery: CacheQuery = {
          exchangeCode: instrument.exchange,
          category: instrument.category,
          symbol: instrument.symbol,
          algorithmId: SPIKE_ALGORITHM_ID,
          algorithmVersion: SPIKE_ALGORITHM_VERSION,
          params,
          fromUnix: body.fromUnix,
          toUnix: body.toUnix,
        };

        const run = await db
          .insertInto("replay_runs")
          .values({
            exchange_code: instrument.exchange,
            category: instrument.category,
            symbol: instrument.symbol,
            from_ts: new Date(body.fromUnix * 1000),
            to_ts: new Date(body.toUnix * 1000),
            algorithm_id: SPIKE_ALGORITHM_ID,
            algorithm_version: SPIKE_ALGORITHM_VERSION,
            params,
            status: "running",
            cache_hit: false,
          })
          .returningAll()
          .executeTakeFirstOrThrow();

        const cached = await cacheLookup(db, cacheQuery);
        if (cached !== null) {
          const completed = await db.transaction().execute(async (tx) => {
          const current = await tx.selectFrom("replay_runs").select("status").where("id", "=", run.id).forUpdate().executeTakeFirstOrThrow();
          return tx
            .updateTable("replay_runs")
            .set({
              status: current.status === "cancel_requested" ? "cancelled" : "done",
              cache_hit: true,
              quality: cached.quality,
              quality_source_run_id: cached.qualitySourceRunId,
              quality_computed_at: new Date(cached.computedAt),
              setups: toJsonArray(cached.setups),
              warmup_minutes: cached.warmupMinutes,
              processed_minutes: cached.processedMinutes,
              updated_at: new Date(),
            })
            .where("id", "=", run.id)
            .returningAll().executeTakeFirstOrThrow();
          });
          runs.push(replayRunDto(completed));
          continue;
        }

        try {
          const source = new ApiCandleSource(
            marketData.url,
            marketData.apiKey,
            catalogEntry.id,
          );
          const cancel = { cancelled: false };
          let finalIntervals: QualityInterval[] = [];
          const result = await replayRun({
            cancel,
            onQualityPage: async (quality, intervals, checkpoint) => {
              if (quality.state !== "in_progress") { finalIntervals = intervals; return; }
              await db.transaction().execute(async (tx) => {
                await saveQualityPage(tx, run.id, quality, intervals);
                await tx.updateTable("replay_runs").set({
                  setups: sql<SpikeSetup[]>`coalesce(setups, '[]'::jsonb) || ${JSON.stringify(checkpoint.setups)}::jsonb`,
                  warmup_minutes: checkpoint.warmupMinutes, processed_minutes: checkpoint.processedMinutes,
                  updated_at: new Date(),
                }).where("id", "=", run.id).execute();
              });
            },
            source: { async read(query) {
              const current = await db.selectFrom("replay_runs").select("status").where("id", "=", run.id).executeTakeFirstOrThrow();
              if (current.status === "cancel_requested") { cancel.cancelled = true; return { bars: [], nextCursorUnix: null }; }
              return source.read(query);
            } },
            fromUnix: body.fromUnix,
            toUnix: body.toUnix,
            params,
          });
          const completed = await db.transaction().execute(async (tx) => {
            const current = await tx.selectFrom("replay_runs").select("status").where("id", "=", run.id).forUpdate().executeTakeFirstOrThrow();
            const cancelled = current.status === "cancel_requested";
            const computedAt = new Date();
            if (!cancelled) {
              await saveQualityPage(tx, run.id, result.quality, finalIntervals);
              await lockReplayCache(tx);
              if (await cacheGeneration(tx) === generation) await cacheStore(tx, cacheQuery, { ...result, qualitySourceRunId: run.id, computedAt });
            }
          return tx
            .updateTable("replay_runs")
            .set({
              status: cancelled ? "cancelled" : "done",
              setups: toJsonArray(result.setups),
              warmup_minutes: result.warmupMinutes,
              processed_minutes: result.processedMinutes,
              quality_computed_at: cancelled ? null : computedAt,
              updated_at: new Date(),
            })
            .where("id", "=", run.id)
            .returningAll().executeTakeFirstOrThrow();
          });
          runs.push(replayRunDto(completed));
        } catch (error) {
          const failed = await db
            .updateTable("replay_runs")
            .set({
              status: sql<string>`case when status = 'cancel_requested' then 'cancelled' else 'failed' end`,
              error: error instanceof Error ? error.message : String(error),
              updated_at: new Date(),
            })
            .where("id", "=", run.id)
            .returningAll().executeTakeFirstOrThrow();
          runs.push(replayRunDto(failed));
        }
      }
      await reply.send({ runs });
    },
  );

  app.post("/v1/replay-runs/:id/cancel", {
    preHandler: [requireApiKey],
    schema: { params: Type.Object({ id: Type.Integer({ minimum: 1 }) }) },
  }, async (request, reply) => {
    const run = await cancelReplayRun(db, (request.params as { id: number }).id);
    if (!run) return reply.code(404).send({ error: "run not found" });
    return reply.send({ run: replayRunDto(run) });
  });

  app.get("/v1/settings/replay", { preHandler: [requireApiKey] }, async () => ({ paused: (await getSetting(db, "replay")).paused === true }));
  app.put("/v1/settings/replay", {
    preHandler: [requireApiKey], schema: { body: Type.Object({ paused: Type.Boolean() }) },
  }, async (request) => {
    const body = request.body as { paused: boolean };
    await setSetting(db, "replay", body);
    return body;
  });

  app.get(
    "/v1/replay-runs",
    {
      preHandler: [requireApiKey],
      schema: { response: { 200: Type.Object({ runs: Type.Array(RunDto) }) } },
    },
    async (_request, reply) => {
      const rows = await db
        .selectFrom("replay_runs")
        .select(["id", "exchange_code", "category", "symbol", "status", "cache_hit", "warmup_minutes",
          "processed_minutes", "error", "algorithm_id", "algorithm_version", "from_ts", "to_ts", "created_at", "quality", "quality_source_run_id", "quality_computed_at"])
        .select(sql<number>`coalesce(jsonb_array_length(setups), 0)`.as("setup_count"))
        .orderBy("id", "desc")
        .limit(200)
        .execute();
      await reply.send({
        runs: rows.map((row) => ({
          quality: row.quality, qualitySourceRunId: row.quality_source_run_id,
          qualityComputedAt: row.quality_computed_at?.toISOString() ?? null,
          id: row.id,
          exchange: row.exchange_code,
          category: row.category,
          symbol: row.symbol,
          status: row.status,
          cacheHit: row.cache_hit,
          setupCount: row.setup_count,
          warmupMinutes: row.warmup_minutes,
          processedMinutes: row.processed_minutes,
          error: row.error,
          algorithmId: row.algorithm_id,
          algorithmVersion: row.algorithm_version,
          fromUnix: Math.floor(row.from_ts.getTime() / 1000),
          toUnix: Math.floor(row.to_ts.getTime() / 1000),
          createdAt: row.created_at.toISOString(),
        })),
      });
    },
  );

  app.get(
    "/v1/replay-runs/:id",
    {
      preHandler: [requireApiKey],
      schema: {
        params: Type.Object({ id: Type.Integer() }),
        querystring: Type.Object({
          offset: Type.Optional(Type.Integer({ minimum: 0, maximum: Number.MAX_SAFE_INTEGER - 500, default: 0 })),
          limit: Type.Optional(Type.Integer({ minimum: 1, maximum: 500, default: 100 })),
        }),
        response: {
          200: Type.Object({ run: RunDto, setups: Type.Array(SetupDto), offset: Type.Integer(), limit: Type.Integer(), total: Type.Integer() }),
          404: ErrorEnvelope,
        },
      },
    },
    async (request, reply) => {
      const { id } = request.params as { id: number };
      const { offset = 0, limit = 100 } = request.query as { offset?: number; limit?: number };
      const row = await replayResultPage(db, id, offset, limit);
      if (row === undefined) {
        await reply.code(404).send({ error: "run not found" });
        return;
      }
      const setups = Array.isArray(row.setups) ? row.setups : [];
      await reply.send({
        offset,
        limit,
        total: row.setup_count,
        run: {
          quality: row.quality, qualitySourceRunId: row.quality_source_run_id,
          qualityComputedAt: row.quality_computed_at?.toISOString() ?? null,
          id: row.id,
          exchange: row.exchange_code,
          category: row.category,
          symbol: row.symbol,
          status: row.status,
          cacheHit: row.cache_hit,
          setupCount: row.setup_count,
          warmupMinutes: row.warmup_minutes,
          processedMinutes: row.processed_minutes,
          error: row.error,
          algorithmId: row.algorithm_id,
          algorithmVersion: row.algorithm_version,
          fromUnix: Math.floor(row.from_ts.getTime() / 1000),
          toUnix: Math.floor(row.to_ts.getTime() / 1000),
          createdAt: row.created_at.toISOString(),
          params: row.params,
        },
        setups: setups.map((setup: SpikeSetup) => ({
          exchange: row.exchange_code,
          category: row.category,
          symbol: row.symbol,
          algorithmId: setup.algorithmId,
          algorithmVersion: setup.algorithmVersion,
          params: setup.params,
          details: setup.details ?? null,
          openTimeUnix: setup.openTimeUnix,
          closeTimeUnix: setup.closeTimeUnix,
          closePrice: setup.closePrice,
        })),
      });
    },
  );

  app.get("/v1/replay-runs/:id/quality-intervals", {
    preHandler: [requireApiKey],
    schema: {
      params: Type.Object({ id: Type.Integer({ minimum: 1 }) }),
      querystring: Type.Object({ offset: Type.Optional(Type.Integer({ minimum: 0, maximum: Number.MAX_SAFE_INTEGER })),
        limit: Type.Optional(Type.Integer({ minimum: 1, maximum: 500 })) }),
    },
  }, async (request, reply) => {
    const { id } = request.params as { id: number };
    const { offset = 0, limit = 100 } = request.query as { offset?: number; limit?: number };
    // One snapshot keeps counts, ownership and page consistent during cancellation/deletion.
    return db.transaction().setIsolationLevel("repeatable read").execute(async (tx) => {
      const run = await tx.selectFrom("replay_runs").select(["quality", "quality_source_run_id"])
        .where("id", "=", id).executeTakeFirst();
      if (!run) return reply.code(404).send({ error: "run not found" });
      if (!run.quality || run.quality_source_run_id === null) return { intervals: [], offset, limit, total: 0, available: false };
      const base = tx.selectFrom("replay_quality_intervals").where("run_id", "=", run.quality_source_run_id);
      const count = await base.select((eb) => eb.fn.countAll().as("count")).executeTakeFirstOrThrow();
      const intervals = await base.select(["from_unix", "to_unix", "reason"])
        .orderBy("from_unix").offset(offset).limit(limit).execute();
      return { intervals: intervals.map((interval) => ({ fromUnix: Number(interval.from_unix),
        toUnix: Number(interval.to_unix), reason: interval.reason })), offset, limit, total: Number(count.count), available: true };
    });
  });

  app.get("/v1/replay-runs/:id/export.csv", {
    preHandler: [requireApiKey],
    schema: { params: Type.Object({ id: Type.Integer({ minimum: 1 }) }) },
  }, async (request, reply) => {
    const { id } = request.params as { id: number };
    const first = await replayResultPage(db, id, 0, 500);
    if (!first) return reply.code(404).send({ error: "run not found" });
    // Freeze the initial count: an active run can append results during export.
    const total = first.setup_count;
    async function* chunks() {
      yield "signal,exchange,category,symbol,algorithmId,algorithmVersion,openTimeUnix,closeTimeUnix,closePrice,growthPct,volumeLongRatio,volumeShortRatio,overLong,overShort,missingMinutes,params\r\n";
      let page = first;
      for (let offset = 0; offset < total; offset += 500) {
        if (offset > 0) page = await replayResultPage(db, id, offset, Math.min(500, total - offset));
        if (!page) throw new Error("saved replay disappeared during export");
        for (const setup of page.setups) {
          const details = setup.details;
          yield ["WATCH", first!.exchange_code, first!.category, first!.symbol,
            setup.algorithmId ?? first!.algorithm_id, setup.algorithmVersion ?? first!.algorithm_version,
            setup.openTimeUnix, setup.closeTimeUnix, setup.closePrice,
            details?.growthPct, details?.volumeLongRatio, details?.volumeShortRatio,
            details?.overLong, details?.overShort, details?.missingMinutes,
            JSON.stringify(setup.params ?? first!.params)].map(csvCell).join(",") + "\r\n";
        }
      }
    }
    return reply.type("text/csv; charset=utf-8")
      .header("content-disposition", `attachment; filename="replay-${id}.csv"`)
      .send(Readable.from(chunks(), { objectMode: false, highWaterMark: 64 * 1024 }));
  });

  app.get(
    "/v1/setups",
    {
      preHandler: [requireApiKey],
      schema: {
        querystring: Type.Object({
          offset: Type.Optional(Type.Integer({ minimum: 0, maximum: 2_147_483_647 })),
          limit: Type.Optional(Type.Integer({ minimum: 1, maximum: 500 })),
          snapshotId: Type.Optional(Type.Integer({ minimum: 0, maximum: 2_147_483_647 })),
          q: Type.Optional(Type.String({ maxLength: 200 })),
        }),
        response: { 200: Type.Object({ setups: Type.Array(SetupDto), offset: Type.Integer(),
          limit: Type.Integer(), total: Type.Integer(), snapshotId: Type.Integer() }) },
      },
    },
    async (request, reply) => {
      const { offset = 0, limit = 500, q = "", snapshotId: requestedSnapshot } = request.query as {
        offset?: number; limit?: number; q?: string; snapshotId?: number;
      };
      // Keep later appends outside an existing page sequence without holding a DB transaction.
      const snapshotId = requestedSnapshot ?? (await db.selectFrom("live_setups")
        .select((eb) => eb.fn.max("id").as("id")).executeTakeFirstOrThrow()).id ?? 0;
      let query = db.selectFrom("live_setups").where("id", "<=", snapshotId);
      if (q.trim()) query = query.where(sql<boolean>`strpos(lower(concat_ws(' ', symbol, exchange_code, algorithm_id)), lower(${q.trim()})) > 0`);
      const count = await query.select((eb) => eb.fn.countAll<string>().as("total")).executeTakeFirstOrThrow();
      const rows = await query.selectAll().orderBy("close_time", "desc").orderBy("id", "desc")
        .offset(offset).limit(limit).execute();
      await reply.send({
        offset, limit, total: Number(count.total), snapshotId,
        setups: rows.map((row) => ({
          exchange: row.exchange_code,
          category: row.category,
          symbol: row.symbol,
          algorithmId: row.algorithm_id,
          algorithmVersion: row.algorithm_version,
          openTimeUnix: Math.floor(row.open_time.getTime() / 1000),
          params: row.params,
          details: row.details ?? null,
          closeTimeUnix: Math.floor(row.close_time.getTime() / 1000),
          closePrice: row.close_price,
        })),
      });
    },
  );

  app.get(
    "/v1/export/setups.csv",
    { preHandler: [requireApiKey] },
    async (_request, reply) => {
      const snapshotId = (await db.selectFrom("live_setups")
        .select((eb) => eb.fn.max("id").as("id")).executeTakeFirstOrThrow()).id ?? 0;
      const query = db.selectFrom("live_setups")
        .select(["id", "exchange_code", "category", "symbol", "algorithm_id", "algorithm_version", "open_time", "close_time", "close_price"])
        .where("id", "<=", snapshotId).orderBy("close_time", "asc").orderBy("id", "asc").limit(500);
      // Fail before sending headers if the first read fails. No cursor/transaction stays open.
      let rows = await query.execute();
      async function* chunks() {
        yield "exchange,category,symbol,algorithmId,algorithmVersion,openTimeUnix,closeTimeUnix,closePrice\r\n";
        while (rows.length && !reply.raw.destroyed) {
          for (const row of rows) {
            if (reply.raw.destroyed) return;
            yield [row.exchange_code, row.category, row.symbol, row.algorithm_id, row.algorithm_version,
              Math.floor(row.open_time.getTime() / 1000), Math.floor(row.close_time.getTime() / 1000), row.close_price]
              .map(csvCell).join(",") + "\r\n";
          }
          if (rows.length < 500 || reply.raw.destroyed) return;
          const last = rows[rows.length - 1]!;
          rows = await query.where(sql<boolean>`(close_time, id) > (${last.close_time}, ${last.id})`).execute();
        }
      }
      return reply.type("text/csv; charset=utf-8")
        .header("Content-Disposition", 'attachment; filename="live-setups.csv"')
        .header("Cache-Control", "no-store")
        .header("X-Accel-Buffering", "no")
        .send(Readable.from(chunks(), { objectMode: false, highWaterMark: 16 * 1024 }));
    },
  );

  app.get(
    "/v1/settings/notifications",
    { preHandler: [requireApiKey] },
    async (_request, reply) => {
      const maxAgeSeconds = await getNotificationMaxAgeSeconds(db);
      await reply.send({ maxAgeSeconds });
    },
  );

  app.put(
    "/v1/settings/notifications",
    {
      preHandler: [requireApiKey],
      schema: {
        body: Type.Object({ maxAgeSeconds: Type.Integer({ minimum: 1, maximum: 86_400 }) }),
      },
    },
    async (request, reply) => {
      const body = request.body as { maxAgeSeconds: number };
      await setNotificationMaxAgeSeconds(db, body.maxAgeSeconds);
      await reply.send({ maxAgeSeconds: body.maxAgeSeconds });
    },
  );

  app.get(
    "/v1/replay-cache/stats",
    { preHandler: [requireApiKey] },
    async (_request, reply) => {
      await reply.send(await cacheStats(db));
    },
  );

  app.delete(
    "/v1/replay-cache/:exchange/:category/:symbol",
    {
      preHandler: [requireApiKey],
      schema: {
        params: Type.Object({
          exchange: Type.String(),
          category: Type.String(),
          symbol: Type.String(),
        }),
      },
    },
    async (request, reply) => {
      const params = request.params as {
        exchange: string;
        category: string;
        symbol: string;
      };
      const removed = await cacheClearByInstrument(db, {
        exchangeCode: params.exchange,
        category: params.category,
        symbol: params.symbol,
      });
      await reply.send({ removed });
    },
  );

  app.delete("/v1/replay-cache", { preHandler: [requireApiKey] }, async (_request, reply) => {
    const removed = await cacheClearAll(db);
    await reply.send({ removed });
  });
}
