import type { FastifyInstance, FastifyReply, FastifyRequest } from "fastify";
import { Readable } from "node:stream";
import { Type } from "@sinclair/typebox";
import { sql, type Kysely } from "kysely";
import type { SignalsDB } from "../db/schema.js";
import type { SpikeParams, SpikeSetup } from "../detector/price-volume-spike.js";
import { SPIKE_ALGORITHM_ID, SPIKE_ALGORITHM_VERSION } from "../detector/price-volume-spike.js";
import { getAlgorithm, SpikeParamsSchema, validateSpikeParams } from "../algorithms.js";
import { registerControls } from "./controls.js";
import { ApiCandleSource } from "../replay/source.js";
import { replayRun } from "../replay/replay-runner.js";
import { cancelReplayRun, enqueueReplayRuns, replayRunDto } from "../replay/jobs.js";
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
      "processed_minutes", "error", "algorithm_id", "algorithm_version", "from_ts", "to_ts", "created_at", "params"])
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
      const body = request.body as {
        instruments: { exchange: string; category: string; symbol: string }[];
        fromUnix: number;
        toUnix: number;
        algorithmId?: string;
        params?: SpikeParams;
        async?: boolean;
      };
      const algorithm = await getAlgorithm(db, body.algorithmId);
      if (!algorithm) return reply.code(400).send({ error: "unknown_algorithm" });
      const params = { ...algorithm.params, ...body.params };
      const error = validateSpikeParams(params);
      if (error || !Number.isSafeInteger(body.fromUnix) || !Number.isSafeInteger(body.toUnix) ||
        body.fromUnix < 0 || body.toUnix > 253_402_300_740 ||
        body.fromUnix >= body.toUnix || body.toUnix - body.fromUnix > 366 * 86400) {
        return reply.code(400).send({ error: error ?? "invalid_range_max_366_days" });
      }

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
          const result = await replayRun({
            source: { async read(query) {
              const current = await db.selectFrom("replay_runs").select("status").where("id", "=", run.id).executeTakeFirstOrThrow();
              if (current.status === "cancel_requested") return { bars: [], nextCursorUnix: null };
              return source.read(query);
            } },
            fromUnix: body.fromUnix,
            toUnix: body.toUnix,
            params,
          });
          const completed = await db.transaction().execute(async (tx) => {
            const current = await tx.selectFrom("replay_runs").select("status").where("id", "=", run.id).forUpdate().executeTakeFirstOrThrow();
            const cancelled = current.status === "cancel_requested";
            if (!cancelled) {
              await lockReplayCache(tx);
              if (await cacheGeneration(tx) === generation) await cacheStore(tx, cacheQuery, result);
            }
          return tx
            .updateTable("replay_runs")
            .set({
              status: cancelled ? "cancelled" : "done",
              setups: toJsonArray(result.setups),
              warmup_minutes: result.warmupMinutes,
              processed_minutes: result.processedMinutes,
              updated_at: new Date(),
            })
            .where("id", "=", run.id)
            .returningAll().executeTakeFirstOrThrow();
          });
          runs.push({
            id: run.id,
            exchange: run.exchange_code,
            category: run.category,
            symbol: run.symbol,
            status: completed.status,
            cacheHit: false,
            setupCount: result.setups.length,
            warmupMinutes: result.warmupMinutes,
            processedMinutes: result.processedMinutes,
            error: null,
          });
        } catch (error) {
          await db
            .updateTable("replay_runs")
            .set({
              status: sql<string>`case when status = 'cancel_requested' then 'cancelled' else 'failed' end`,
              error: error instanceof Error ? error.message : String(error),
              updated_at: new Date(),
            })
            .where("id", "=", run.id)
            .execute();
          runs.push({
            id: run.id,
            exchange: run.exchange_code,
            category: run.category,
            symbol: run.symbol,
            status: "failed",
            cacheHit: false,
            setupCount: 0,
            warmupMinutes: null,
            processedMinutes: null,
            error: error instanceof Error ? error.message : String(error),
          });
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
          "processed_minutes", "error", "algorithm_id", "algorithm_version", "from_ts", "to_ts", "created_at"])
        .select(sql<number>`coalesce(jsonb_array_length(setups), 0)`.as("setup_count"))
        .orderBy("id", "desc")
        .limit(200)
        .execute();
      await reply.send({
        runs: rows.map((row) => ({
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
        response: { 200: Type.Object({ setups: Type.Array(SetupDto) }) },
      },
    },
    async (_request, reply) => {
      const rows = await db
        .selectFrom("live_setups")
        .selectAll()
        .orderBy("close_time", "desc")
        .limit(500)
        .execute();
      await reply.send({
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
      const rows = await db
        .selectFrom("live_setups")
        .selectAll()
        .orderBy("close_time", "asc")
        .limit(50_000)
        .execute();
      const header =
        "exchange,category,symbol,algorithmId,algorithmVersion,openTimeUnix,closeTimeUnix,closePrice";
      const lines = rows.map(
        (row) =>
          `${row.exchange_code},${row.category},${row.symbol},${row.algorithm_id},` +
          `${row.algorithm_version},${Math.floor(row.open_time.getTime() / 1000)},` +
          `${Math.floor(row.close_time.getTime() / 1000)},${row.close_price}`,
      );
      await reply.type("text/csv; charset=utf-8").send(`${[header, ...lines].join("\n")}\n`);
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
