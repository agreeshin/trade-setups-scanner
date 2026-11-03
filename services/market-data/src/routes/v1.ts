import type { FastifyInstance, FastifyReply, FastifyRequest } from "fastify";
import { Type } from "@sinclair/typebox";
import type { Kysely } from "kysely";
import type { MarketDataDB } from "../db/schema.js";
import { readCandles } from "../storage/candles.js";
import { getCoverage } from "../storage/coverage.js";

export type ApiKeyGuard = (
  request: FastifyRequest,
  reply: FastifyReply,
) => Promise<void>;

const InstrumentDto = Type.Object({
  id: Type.Integer(),
  exchange: Type.String(),
  category: Type.String(),
  symbol: Type.String(),
  status: Type.String(),
});

const InstrumentsResponse = Type.Object({
  instruments: Type.Array(InstrumentDto),
});

const IntervalDto = Type.Object({
  startUnix: Type.Integer(),
  endUnix: Type.Integer(),
});

const CoverageResponse = Type.Object({
  instrumentId: Type.Integer(),
  firstMinuteUnix: Type.Union([Type.Integer(), Type.Null()]),
  lastMinuteUnix: Type.Union([Type.Integer(), Type.Null()]),
  totalCandles: Type.Integer(),
  intervals: Type.Array(IntervalDto),
  gaps: Type.Array(IntervalDto),
});

const CandleDto = Type.Object({
  instrumentId: Type.Integer(),
  openTimeUnix: Type.Integer(),
  open: Type.String(),
  high: Type.String(),
  low: Type.String(),
  close: Type.String(),
  volume: Type.String(),
  quoteTurnover: Type.String(),
  source: Type.String(),
});

const CandlesResponse = Type.Object({
  candles: Type.Array(CandleDto),
  nextCursorUnix: Type.Union([Type.Integer(), Type.Null()]),
});

const ErrorEnvelope = Type.Object({ error: Type.String() });

const unixSeconds = (iso: string | null): number | null =>
  iso === null ? null : Math.floor(new Date(iso).getTime() / 1000);

export interface V1RouteOptions {
  db: Kysely<MarketDataDB>;
  requireApiKey: ApiKeyGuard;
}

export function registerV1Routes(
  app: FastifyInstance,
  options: V1RouteOptions,
): void {
  const { db, requireApiKey } = options;

  app.get(
    "/v1/instruments",
    {
      preHandler: [requireApiKey],
      schema: {
        response: { 200: InstrumentsResponse, 401: ErrorEnvelope },
      },
    },
    async (_request, reply) => {
      const rows = await db
        .selectFrom("instruments")
        .innerJoin("exchanges", "exchanges.id", "instruments.exchange_id")
        .select([
          "instruments.id as id",
          "exchanges.code as exchange",
          "instruments.category as category",
          "instruments.symbol as symbol",
          "instruments.status as status",
        ])
        .orderBy("exchanges.code", "asc")
        .orderBy("instruments.symbol", "asc")
        .execute();
      await reply.send({
        instruments: rows.map((row) => ({
          id: row.id,
          exchange: row.exchange,
          category: row.category,
          symbol: row.symbol,
          status: row.status,
        })),
      });
    },
  );

  app.get(
    "/v1/instruments/:instrumentId/coverage",
    {
      preHandler: [requireApiKey],
      schema: {
        params: Type.Object({ instrumentId: Type.Integer() }),
        response: { 200: CoverageResponse, 401: ErrorEnvelope },
      },
    },
    async (request, reply) => {
      const params = request.params as { instrumentId: number };
      const coverage = await getCoverage(db, params.instrumentId);
      await reply.send({
        instrumentId: params.instrumentId,
        firstMinuteUnix: unixSeconds(coverage.firstMinute),
        lastMinuteUnix: unixSeconds(coverage.lastMinute),
        totalCandles: coverage.totalCandles,
        intervals: coverage.intervals.map((interval) => ({
          startUnix: unixSeconds(interval.start) ?? 0,
          endUnix: unixSeconds(interval.end) ?? 0,
        })),
        gaps: coverage.gaps.map((gap) => ({
          startUnix: unixSeconds(gap.start) ?? 0,
          endUnix: unixSeconds(gap.end) ?? 0,
        })),
      });
    },
  );

  app.get(
    "/v1/candles",
    {
      preHandler: [requireApiKey],
      schema: {
        querystring: Type.Object({
          instrumentId: Type.Integer(),
          fromUnix: Type.Integer(),
          toUnix: Type.Integer(),
          limit: Type.Optional(Type.Integer({ minimum: 1, maximum: 5000 })),
          afterUnix: Type.Optional(Type.Integer()),
        }),
        response: { 200: CandlesResponse, 401: ErrorEnvelope },
      },
    },
    async (request, reply) => {
      const query = request.query as {
        instrumentId: number;
        fromUnix: number;
        toUnix: number;
        limit?: number;
        afterUnix?: number;
      };
      const page = await readCandles(db, {
        instrumentId: query.instrumentId,
        from: new Date(query.fromUnix * 1000),
        to: new Date(query.toUnix * 1000),
        limit: query.limit,
        afterUnix: query.afterUnix,
      });
      await reply.send({
        candles: page.candles.map((candle) => ({
          instrumentId: candle.instrumentId,
          openTimeUnix: candle.openTimeUnix,
          open: candle.open,
          high: candle.high,
          low: candle.low,
          close: candle.close,
          volume: candle.volume,
          quoteTurnover: candle.quoteTurnover,
          source: candle.source,
        })),
        nextCursorUnix: page.nextCursorUnix,
      });
    },
  );
}
