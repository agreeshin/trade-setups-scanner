import type { FastifyInstance, FastifyReply, FastifyRequest } from "fastify";
import { Type } from "@sinclair/typebox";
import type { Kysely } from "kysely";
import type { SignalsDB } from "../db/schema.js";
import type { SpikeParams, SpikeSetup } from "../detector/price-volume-spike.js";
import { DEFAULT_SPIKE_PARAMS, SPIKE_ALGORITHM_ID, SPIKE_ALGORITHM_VERSION } from "../detector/price-volume-spike.js";
import { ApiCandleSource } from "../replay/source.js";
import { replayRun } from "../replay/replay-runner.js";
import {
  cacheClearAll,
  cacheClearByInstrument,
  cacheLookup,
  cacheStats,
  cacheStore,
  type CacheQuery,
} from "../replay/cache.js";
import { fetchCatalog } from "../platform-catalog.js";
import {
  getNotificationMaxAgeSeconds,
  setNotificationMaxAgeSeconds,
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
  fromUnix: Type.Integer(),
  toUnix: Type.Integer(),
  params: Type.Optional(Type.Object({})),
});

const SetupDto = Type.Object({
  exchange: Type.String(),
  category: Type.String(),
  symbol: Type.String(),
  algorithmId: Type.String(),
  algorithmVersion: Type.String(),
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
});

const toJsonArray = <T>(value: T[]): T[] => JSON.stringify(value) as unknown as T[];

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

  app.post(
    "/v1/replay-runs",
    {
      preHandler: [requireApiKey],
      schema: {
        body: ReplayRunRequest,
        response: { 200: Type.Object({ runs: Type.Array(RunDto) }), 401: ErrorEnvelope },
      },
    },
    async (request, reply) => {
      const body = request.body as {
        instruments: { exchange: string; category: string; symbol: string }[];
        fromUnix: number;
        toUnix: number;
        params?: SpikeParams;
      };
      const params = { ...DEFAULT_SPIKE_PARAMS, ...body.params };

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
          await db
            .updateTable("replay_runs")
            .set({
              status: "done",
              cache_hit: true,
              setups: toJsonArray(cached.setups),
              warmup_minutes: cached.warmupMinutes,
              processed_minutes: cached.processedMinutes,
              updated_at: new Date(),
            })
            .where("id", "=", run.id)
            .execute();
          runs.push({
            id: run.id,
            exchange: run.exchange_code,
            category: run.category,
            symbol: run.symbol,
            status: "done",
            cacheHit: true,
            setupCount: cached.setups.length,
            warmupMinutes: cached.warmupMinutes,
            processedMinutes: cached.processedMinutes,
            error: null,
          });
          continue;
        }

        try {
          const source = new ApiCandleSource(
            marketData.url,
            marketData.apiKey,
            catalogEntry.id,
          );
          const result = await replayRun({
            source,
            fromUnix: body.fromUnix,
            toUnix: body.toUnix,
            params,
          });
          await cacheStore(db, cacheQuery, {
            setups: result.setups,
            warmupMinutes: result.warmupMinutes,
            processedMinutes: result.processedMinutes,
          });
          await db
            .updateTable("replay_runs")
            .set({
              status: "done",
              setups: toJsonArray(result.setups),
              warmup_minutes: result.warmupMinutes,
              processed_minutes: result.processedMinutes,
              updated_at: new Date(),
            })
            .where("id", "=", run.id)
            .execute();
          runs.push({
            id: run.id,
            exchange: run.exchange_code,
            category: run.category,
            symbol: run.symbol,
            status: "done",
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
              status: "failed",
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

  app.get(
    "/v1/replay-runs",
    {
      preHandler: [requireApiKey],
      schema: { response: { 200: Type.Object({ runs: Type.Array(RunDto) }) } },
    },
    async (_request, reply) => {
      const rows = await db
        .selectFrom("replay_runs")
        .selectAll()
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
          setupCount: Array.isArray(row.setups) ? row.setups.length : 0,
          warmupMinutes: row.warmup_minutes,
          processedMinutes: row.processed_minutes,
          error: row.error,
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
        response: {
          200: Type.Object({ run: RunDto, setups: Type.Array(SetupDto) }),
          404: ErrorEnvelope,
        },
      },
    },
    async (request, reply) => {
      const { id } = request.params as { id: number };
      const row = await db
        .selectFrom("replay_runs")
        .where("id", "=", id)
        .selectAll()
        .executeTakeFirst();
      if (row === undefined) {
        await reply.code(404).send({ error: "run not found" });
        return;
      }
      const setups = Array.isArray(row.setups) ? row.setups : [];
      await reply.send({
        run: {
          id: row.id,
          exchange: row.exchange_code,
          category: row.category,
          symbol: row.symbol,
          status: row.status,
          cacheHit: row.cache_hit,
          setupCount: setups.length,
          warmupMinutes: row.warmup_minutes,
          processedMinutes: row.processed_minutes,
          error: row.error,
        },
        setups: setups.map((setup: SpikeSetup) => ({
          exchange: row.exchange_code,
          category: row.category,
          symbol: row.symbol,
          algorithmId: setup.algorithmId,
          algorithmVersion: setup.algorithmVersion,
          openTimeUnix: setup.openTimeUnix,
          closeTimeUnix: setup.closeTimeUnix,
          closePrice: setup.closePrice,
        })),
      });
    },
  );

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
