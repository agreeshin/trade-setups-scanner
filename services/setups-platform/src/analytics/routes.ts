import type { FastifyInstance, FastifyReply, FastifyRequest } from "fastify";
import { Type } from "@sinclair/typebox";
import type { Kysely } from "kysely";
import type { SignalsDB } from "../db/schema.js";
import { MarketDataCandleReader } from "./candles.js";
import {
  aggregateLimitOutcomes,
  DEFAULT_CLUSTER_GAP_HOURS,
  DEFAULT_HORIZON_MINUTES,
  dayRangeToUnixBounds,
  evaluateLimit,
  filterByDayRange,
  filterByMonth,
  passesPriceChange,
  resolveEntry,
  riskReturn,
  selectByMaturity,
  setupMonths,
  type AnalyticsSetup,
  type LimitEvaluation,
  type OhlcCandle,
  type TradeDirection,
} from "./engine.js";
import {
  analyticsCacheSummary,
  clearAnalyticsCache,
  getMatrixCell,
  getSelection,
  getTradeRow,
  listMatrixCells,
  listTradeRows,
  matrixScopeKey,
  maturityTenthsOf,
  priceChangePpmOf,
  saveMatrixCell,
  saveSelection,
  saveTradeRow,
  selectionScopeKey,
  type CachedPayload,
  type ComputeParams,
  type SetScope,
} from "./cache.js";
import {
  listAnalyticsSets,
  loadAnalyticsSetups,
  parseSetId,
  setupsFingerprint,
  type SetRef,
} from "./sets.js";

/** Hard cap protecting one request from unbounded setup sets. */
const MAX_SETUPS_PER_COMPUTE = 100_000;

const DEFAULT_CLUSTER_GAP_MINUTES = DEFAULT_CLUSTER_GAP_HOURS * 60;

const ErrorEnvelope = Type.Object({ error: Type.String() });

const SetIdParam = Type.String({ pattern: "^(batch|run)-[1-9][0-9]*$" });
const DirectionParam = Type.Union([Type.Literal("short"), Type.Literal("long")]);
const MonthParam = Type.String({ pattern: "^\\d{4}-(0[1-9]|1[0-2])$" });
const DayParam = Type.String({ pattern: "^\\d{4}-\\d{2}-\\d{2}$" });

const NullableNumber = Type.Union([Type.Number(), Type.Null()]);

const SetupDto = Type.Object({
  exchange: Type.String(),
  category: Type.String(),
  symbol: Type.String(),
  openTimeUnix: Type.Integer(),
  closeTimeUnix: Type.Integer(),
  closePrice: Type.String(),
});

const TradeRowDto = Type.Object({
  setup: SetupDto,
  direction: Type.Union([Type.Literal("short"), Type.Literal("long")]),
  tpPct: Type.Number(),
  slPct: Type.Number(),
  status: Type.String(),
  entryPrice: NullableNumber,
  tpPrice: NullableNumber,
  slPrice: NullableNumber,
  closeTimeUnix: NullableNumber,
  riskReturn: NullableNumber,
});

const AggregateShape = {
  tpPct: Type.Number(),
  slPct: Type.Number(),
  direction: Type.Union([Type.Literal("short"), Type.Literal("long")]),
  month: Type.Union([MonthParam, Type.Null()]),
  setupMaturity: NullableNumber,
  timeShiftMinutes: Type.Integer(),
  horizonMinutes: Type.Integer(),
  priceChangePct: NullableNumber,
  totalMatchingSetups: Type.Integer(),
  filteredSetupsCount: Type.Integer(),
  eligibleSetupsCount: Type.Integer(),
  priceChangeFilteredCount: Type.Integer(),
  avgSelectionOrdinal: NullableNumber,
  avgSelectionDelayMinutes: NullableNumber,
  totalClustersCount: Type.Union([Type.Integer(), Type.Null()]),
  closedCount: Type.Integer(),
  tpCount: Type.Integer(),
  slCount: Type.Integer(),
  unclosedCount: Type.Integer(),
  noDataCount: Type.Integer(),
  avgMovePct: NullableNumber,
  medianMovePct: NullableNumber,
  winRatePct: NullableNumber,
  avgCloseMinutes: NullableNumber,
  medianCloseMinutes: NullableNumber,
  riskExpectancy: NullableNumber,
};

const AnalyticsQuery = {
  set: SetIdParam,
  direction: Type.Optional(DirectionParam),
  month: Type.Optional(MonthParam),
  fromDay: Type.Optional(DayParam),
  toDay: Type.Optional(DayParam),
  setupMaturity: Type.Optional(Type.Number({ minimum: 0, maximum: 1 })),
  clusterGapHours: Type.Optional(Type.Number({ minimum: 0.01, maximum: 168 })),
  tpPct: Type.Optional(Type.Number({ minimum: 0.000001, maximum: 100 })),
  slPct: Type.Optional(Type.Number({ minimum: 0.000001, maximum: 100 })),
  timeShiftMinutes: Type.Optional(Type.Integer({ minimum: 0, maximum: 43_200, default: 0 })),
  horizonMinutes: Type.Optional(Type.Integer({ minimum: 1, maximum: 43_200, default: DEFAULT_HORIZON_MINUTES })),
  priceChangePct: Type.Optional(Type.Number({ minimum: -100, maximum: 1000 })),
};

interface AnalyticsQueryInput {
  set: string;
  direction?: TradeDirection;
  month?: string;
  fromDay?: string;
  toDay?: string;
  setupMaturity?: number;
  clusterGapHours?: number;
  tpPct?: number;
  slPct?: number;
  timeShiftMinutes?: number;
  horizonMinutes?: number;
  priceChangePct?: number;
}

export type TimeFilter =
  | { kind: "all" }
  | { kind: "month"; monthKey: string }
  | { kind: "range"; fromDay: string; toDay: string };

function filterSetupsByTime(setups: AnalyticsSetup[], filter: TimeFilter): AnalyticsSetup[] {
  if (filter.kind === "month") return filterByMonth(setups, filter.monthKey);
  if (filter.kind === "range") return filterByDayRange(setups, filter.fromDay, filter.toDay);
  return setups;
}

interface ParsedAnalyticsParams {
  setRef: SetRef;
  direction: TradeDirection;
  timeFilter: TimeFilter;
  /** Cache dimension of the time filter: "all", "YYYY-MM" or "d:FROM:TO". */
  cacheTimeKey: string;
  maturity: number | null;
  maturityTenths: number;
  clusterGapMinutes: number;
  tpPct: number;
  slPct: number;
  timeShiftMinutes: number;
  horizonMinutes: number;
  priceChangePct: number | null;
  priceChangePpm: number;
  compute: ComputeParams;
}

function parseAnalyticsQuery(query: AnalyticsQueryInput, requireTpSl: boolean): ParsedAnalyticsParams | { error: string } {
  const setRef = parseSetId(query.set);
  if (setRef === null) return { error: "invalid set id" };
  if (query.setupMaturity !== undefined) {
    const tenths = query.setupMaturity * 10;
    if (!Number.isInteger(tenths) || tenths < 0 || tenths > 10) {
      return { error: "setupMaturity must be 0.0..1.0 with step 0.1" };
    }
  }
  if (requireTpSl && (query.tpPct === undefined || query.slPct === undefined)) {
    return { error: "tpPct and slPct are required" };
  }
  const clusterGapMinutes = query.clusterGapHours !== undefined ? Math.round(query.clusterGapHours * 60) : DEFAULT_CLUSTER_GAP_MINUTES;
  if (clusterGapMinutes < 1 || clusterGapMinutes > 168 * 60) {
    return { error: "clusterGapHours must be 0.01..168" };
  }
  if ((query.fromDay === undefined) !== (query.toDay === undefined)) {
    return { error: "fromDay and toDay must be set together" };
  }
  let timeFilter: TimeFilter = { kind: "all" };
  let cacheTimeKey = "all";
  if (query.fromDay !== undefined && query.toDay !== undefined) {
    const bounds = dayRangeToUnixBounds(query.fromDay, query.toDay);
    if (bounds === null || bounds.fromUnix > bounds.toUnix) {
      return { error: "invalid fromDay/toDay range" };
    }
    timeFilter = { kind: "range", fromDay: query.fromDay, toDay: query.toDay };
    cacheTimeKey = `d:${query.fromDay}:${query.toDay}`;
  } else if (query.month !== undefined) {
    timeFilter = { kind: "month", monthKey: query.month };
    cacheTimeKey = query.month;
  }
  const maturity = query.setupMaturity ?? null;
  const effectiveGapMinutes = maturity !== null ? clusterGapMinutes : -1;
  const priceChangePct = query.priceChangePct ?? null;
  const tpPct = query.tpPct ?? 0;
  const slPct = query.slPct ?? 0;
  const timeShiftMinutes = query.timeShiftMinutes ?? 0;
  const horizonMinutes = query.horizonMinutes ?? DEFAULT_HORIZON_MINUTES;
  const direction = query.direction ?? "short";
  return {
    setRef,
    direction,
    timeFilter,
    cacheTimeKey,
    maturity,
    maturityTenths: maturityTenthsOf(maturity),
    clusterGapMinutes: effectiveGapMinutes,
    tpPct,
    slPct,
    timeShiftMinutes,
    horizonMinutes,
    priceChangePct,
    priceChangePpm: priceChangePpmOf(priceChangePct),
    compute: {
      direction,
      tpPct,
      slPct,
      horizonMinutes,
      timeShiftMinutes,
      priceChangePpm: priceChangePpmOf(priceChangePct),
      maturityTenths: maturityTenthsOf(maturity),
      clusterGapMinutes: effectiveGapMinutes,
    },
  };
}

interface SelectionResult {
  rows: AnalyticsSetup[];
  avgSelectionOrdinal: number | null;
  avgSelectionDelayMinutes: number | null;
  totalClustersCount: number | null;
  cachedSelection: boolean;
}

function identityOf(setup: Pick<AnalyticsSetup, "exchangeCode" | "category" | "symbol" | "closeTimeUnix">): string {
  return `${setup.exchangeCode}\u0000${setup.category}\u0000${setup.symbol}\u0000${setup.closeTimeUnix}`;
}

/** Month filter + maturity selection with a separate selection cache. */
async function selectSetupRows(
  db: Kysely<SignalsDB>,
  setups: AnalyticsSetup[],
  params: ParsedAnalyticsParams,
): Promise<SelectionResult> {
  const inScope = filterSetupsByTime(setups, params.timeFilter);
  if (params.maturity === null) {
    return {
      rows: inScope,
      avgSelectionOrdinal: null,
      avgSelectionDelayMinutes: null,
      totalClustersCount: null,
      cachedSelection: false,
    };
  }
  const fingerprint = setupsFingerprint(setups);
  const scopeKey = selectionScopeKey({
    setKind: params.setRef.kind,
    setId: params.setRef.id,
    setupCount: fingerprint.count,
    firstSetupUnix: fingerprint.first,
    lastSetupUnix: fingerprint.last,
  });
  const cached = await getSelection(db, scopeKey, params.maturityTenths, params.clusterGapMinutes, params.cacheTimeKey).catch(() => undefined);
  if (cached !== undefined && Array.isArray(cached.identities)) {
    const identities = new Set(cached.identities as string[]);
    const rows = inScope.filter((setup) => identities.has(identityOf(setup)));
    return {
      rows,
      avgSelectionOrdinal: (cached.avgSelectionOrdinal as number | null) ?? null,
      avgSelectionDelayMinutes: (cached.avgSelectionDelayMinutes as number | null) ?? null,
      totalClustersCount: (cached.totalClustersCount as number | null) ?? null,
      cachedSelection: true,
    };
  }
  const selection = selectByMaturity(inScope, params.maturity, params.clusterGapMinutes * 60);
  await saveSelection(db, scopeKey, params.maturityTenths, params.clusterGapMinutes, params.cacheTimeKey, {
    identities: selection.rows.map(identityOf),
    avgSelectionOrdinal: selection.avgSelectionOrdinal,
    avgSelectionDelayMinutes: selection.avgSelectionDelayMinutes,
    totalClustersCount: selection.totalClustersCount,
  }).catch(() => undefined);
  return {
    rows: selection.rows,
    avgSelectionOrdinal: selection.avgSelectionOrdinal,
    avgSelectionDelayMinutes: selection.avgSelectionDelayMinutes,
    totalClustersCount: selection.totalClustersCount,
    cachedSelection: false,
  };
}

/** Set fingerprint including the data-side candle signature. */
async function computeScope(
  reader: MarketDataCandleReader,
  setups: AnalyticsSetup[],
  params: ParsedAnalyticsParams,
): Promise<SetScope> {
  const fingerprint = setupsFingerprint(setups);
  let lastCandle = -1;
  if (setups.length > 0) {
    const lastSetup = setups[setups.length - 1];
    if (lastSetup !== undefined) {
      const instrumentId = await reader.instrumentId(lastSetup).catch(() => null);
      if (instrumentId !== null) {
        const from = lastSetup.openTimeUnix + params.timeShiftMinutes * 60;
        const to = from + 60 + params.horizonMinutes * 60;
        const candles = await reader.readWindow(instrumentId, from, to).catch(() => [] as OhlcCandle[]);
        const last = candles[candles.length - 1];
        if (last !== undefined) lastCandle = last.openTimeUnix;
      }
    }
  }
  return {
    setKind: params.setRef.kind,
    setId: params.setRef.id,
    setupCount: fingerprint.count,
    firstSetupUnix: fingerprint.first,
    lastSetupUnix: fingerprint.last,
    lastSetupLastCandleOpenTimeUnix: lastCandle,
  };
}

/** Evaluate selected setups against market-data minute candles. */
async function evaluateRows(
  reader: MarketDataCandleReader,
  rows: AnalyticsSetup[],
  params: ParsedAnalyticsParams,
): Promise<{ evaluations: LimitEvaluation[]; eligibleCount: number; filteredCount: number }> {
  const byInstrument = new Map<string, AnalyticsSetup[]>();
  for (const row of rows) {
    const key = `${row.exchangeCode}\u0000${row.category}\u0000${row.symbol}`;
    const list = byInstrument.get(key);
    if (list === undefined) byInstrument.set(key, [row]);
    else list.push(row);
  }
  const noData: LimitEvaluation = {
    status: "no_data", entryPrice: null, entryTimeUnix: null,
    tpPrice: null, slPrice: null, closeTimeUnix: null, movePct: null, closeMinutes: null,
  };
  const evaluations = new Map<string, LimitEvaluation>();
  let eligibleCount = 0;
  let filteredCount = 0;
  for (const group of byInstrument.values()) {
    const first = group[0];
    if (first === undefined) continue;
    const instrumentId = await reader.instrumentId(first).catch(() => null);
    if (instrumentId === null) {
      for (const setup of group) evaluations.set(identityOf(setup), noData);
      continue;
    }
    const minOpen = group.reduce((min, setup) => Math.min(min, setup.openTimeUnix), Number.MAX_SAFE_INTEGER);
    const maxOpen = group.reduce((max, setup) => Math.max(max, setup.openTimeUnix), 0);
    const window = await reader
      .readWindow(instrumentId, minOpen, maxOpen + params.timeShiftMinutes * 60 + 60 + params.horizonMinutes * 60)
      .catch(() => [] as OhlcCandle[]);
    for (const setup of group) {
      const entry = resolveEntry(window, setup, params.timeShiftMinutes);
      if (entry === null) {
        evaluations.set(identityOf(setup), noData);
        continue;
      }
      if (
        params.priceChangePct !== null &&
        !passesPriceChange(Number(setup.closePrice), entry.entryPrice, params.direction, params.priceChangePct)
      ) {
        filteredCount += 1;
        evaluations.set(identityOf(setup), {
          status: "price_change_filtered", entryPrice: entry.entryPrice, entryTimeUnix: entry.entryCandle.openTimeUnix + 60,
          tpPrice: null, slPrice: null, closeTimeUnix: null, movePct: null, closeMinutes: null,
        });
        continue;
      }
      eligibleCount += 1;
      evaluations.set(identityOf(setup), evaluateLimit({
        candles: window,
        setup,
        direction: params.direction,
        tpPct: params.tpPct,
        slPct: params.slPct,
        timeShiftMinutes: params.timeShiftMinutes,
        horizonMinutes: params.horizonMinutes,
      }));
    }
  }
  const ordered: LimitEvaluation[] = [];
  for (const setup of rows) {
    const evaluation = evaluations.get(identityOf(setup));
    if (evaluation !== undefined) ordered.push(evaluation);
  }
  return { evaluations: ordered, eligibleCount, filteredCount };
}

function aggregatePayload(
  params: ParsedAnalyticsParams,
  selection: SelectionResult,
  totalMatching: number,
  eligibleCount: number,
  filteredCount: number,
  evaluations: LimitEvaluation[],
): CachedPayload {
  const aggregate = aggregateLimitOutcomes(
    evaluations.filter((e) => e.status !== "price_change_filtered"),
    params.slPct,
  );
  return {
    tpPct: params.tpPct,
    slPct: params.slPct,
    direction: params.direction,
    month: params.timeFilter.kind === "month" ? params.timeFilter.monthKey : null,
    setupMaturity: params.maturity,
    timeShiftMinutes: params.timeShiftMinutes,
    horizonMinutes: params.horizonMinutes,
    priceChangePct: params.priceChangePct,
    totalMatchingSetups: totalMatching,
    filteredSetupsCount: selection.rows.length,
    eligibleSetupsCount: eligibleCount,
    priceChangeFilteredCount: filteredCount,
    avgSelectionOrdinal: selection.avgSelectionOrdinal,
    avgSelectionDelayMinutes: selection.avgSelectionDelayMinutes,
    totalClustersCount: selection.totalClustersCount,
    ...aggregate,
  };
}

export interface AnalyticsRouteOptions {
  db: Kysely<SignalsDB>;
  requireApiKey: (request: FastifyRequest, reply: FastifyReply) => Promise<void> | void;
  marketData: { url: string; apiKey: string };
}

export function registerAnalyticsRoutes(app: FastifyInstance, options: AnalyticsRouteOptions): void {
  const { db, requireApiKey } = options;
  const reader = new MarketDataCandleReader(options.marketData.url, options.marketData.apiKey);

  const loadSetOrError = async (parsed: ParsedAnalyticsParams | { error: string }) => {
    if ("error" in parsed) return { error: parsed.error };
    const setups = await loadAnalyticsSetups(db, parsed.setRef);
    if (setups === undefined) return { error: "set not found" };
    return { params: parsed, setups };
  };

  app.get("/v1/analytics/sets", { preHandler: [requireApiKey] }, async () => {
    const sets = await listAnalyticsSets(db);
    return { sets };
  });

  app.get("/v1/analytics/months", {
    preHandler: [requireApiKey],
    schema: {
      querystring: Type.Object({ set: SetIdParam }),
      response: { 200: Type.Object({ months: Type.Array(Type.String()) }), 400: ErrorEnvelope, 404: ErrorEnvelope },
    },
  }, async (request, reply) => {
    const query = request.query as { set: string };
    const ref = parseSetId(query.set);
    if (ref === null) return reply.code(400).send({ error: "invalid set id" });
    const setups = await loadAnalyticsSetups(db, ref);
    if (setups === undefined) return reply.code(404).send({ error: "set not found" });
    return { months: setupMonths(setups) };
  });

  app.get("/v1/analytics/setups", {
    preHandler: [requireApiKey],
    schema: {
      querystring: Type.Object({
        set: SetIdParam,
        setupMaturity: Type.Optional(Type.Number({ minimum: 0, maximum: 1 })),
        month: Type.Optional(MonthParam),
        fromDay: Type.Optional(DayParam),
        toDay: Type.Optional(DayParam),
      }),
      response: {
        200: Type.Object({
          setups: Type.Array(SetupDto),
          totalMatchingSetups: Type.Integer(),
          avgSelectionOrdinal: NullableNumber,
          avgSelectionDelayMinutes: NullableNumber,
          totalClustersCount: Type.Union([Type.Integer(), Type.Null()]),
          cachedSelection: Type.Boolean(),
        }),
        400: ErrorEnvelope,
        404: ErrorEnvelope,
      },
    },
  }, async (request, reply) => {
    const query = request.query as { set: string; setupMaturity?: number; month?: string; fromDay?: string; toDay?: string };
    const parsed = parseAnalyticsQuery({
      set: query.set,
      ...(query.setupMaturity !== undefined ? { setupMaturity: query.setupMaturity } : {}),
      ...(query.month !== undefined ? { month: query.month } : {}),
      ...(query.fromDay !== undefined ? { fromDay: query.fromDay } : {}),
      ...(query.toDay !== undefined ? { toDay: query.toDay } : {}),
    }, false);
    const loaded = await loadSetOrError(parsed);
    if ("error" in loaded) return reply.code(400).send({ error: loaded.error });
    const { params, setups } = loaded;
    const inScope = filterSetupsByTime(setups, params.timeFilter);
    const selection = await selectSetupRows(db, setups, params);
    return {
      setups: selection.rows.map((setup) => ({
        exchange: setup.exchangeCode, category: setup.category, symbol: setup.symbol,
        openTimeUnix: setup.openTimeUnix, closeTimeUnix: setup.closeTimeUnix, closePrice: setup.closePrice,
      })),
      totalMatchingSetups: inScope.length,
      avgSelectionOrdinal: selection.avgSelectionOrdinal,
      avgSelectionDelayMinutes: selection.avgSelectionDelayMinutes,
      totalClustersCount: selection.totalClustersCount,
      cachedSelection: selection.cachedSelection,
    };
  });

  app.get("/v1/analytics/matrix-cells", {
    preHandler: [requireApiKey],
    schema: {
      querystring: Type.Object(AnalyticsQuery),
      response: {
        200: Type.Object({
          items: Type.Array(Type.Object({ tpPct: Type.Number(), slPct: Type.Number(), avgMovePct: NullableNumber, riskExpectancy: NullableNumber })),
          totalMatchingSetups: Type.Integer(),
          filteredSetupsCount: Type.Integer(),
          avgSelectionOrdinal: NullableNumber,
          avgSelectionDelayMinutes: NullableNumber,
        }),
        400: ErrorEnvelope,
        404: ErrorEnvelope,
      },
    },
  }, async (request, reply) => {
    const parsed = parseAnalyticsQuery(request.query as AnalyticsQueryInput, false);
    const loaded = await loadSetOrError(parsed);
    if ("error" in loaded) return reply.code(400).send({ error: loaded.error });
    const { params, setups } = loaded;
    const inScope = filterSetupsByTime(setups, params.timeFilter);
    const selection = await selectSetupRows(db, setups, params);
    const scope = await computeScope(reader, setups, params);
    const rows = await listMatrixCells(db, matrixScopeKey(scope), {
      direction: params.direction,
      horizonMinutes: params.horizonMinutes,
      timeShiftMinutes: params.timeShiftMinutes,
      priceChangePpm: params.priceChangePpm,
      maturityTenths: params.maturityTenths,
      clusterGapMinutes: params.clusterGapMinutes,
    }, params.cacheTimeKey).catch(() => []);
    return {
      items: rows.map((row) => ({
        tpPct: row.tpPct,
        slPct: row.slPct,
        avgMovePct: (row.payload.avgMovePct as number | null) ?? null,
        riskExpectancy: (row.payload.riskExpectancy as number | null) ?? null,
      })),
      totalMatchingSetups: inScope.length,
      filteredSetupsCount: selection.rows.length,
      avgSelectionOrdinal: selection.avgSelectionOrdinal,
      avgSelectionDelayMinutes: selection.avgSelectionDelayMinutes,
    };
  });

  app.get("/v1/analytics/matrix-cell", {
    preHandler: [requireApiKey],
    schema: {
      querystring: Type.Object(AnalyticsQuery),
      response: {
        200: Type.Object({ ...AggregateShape, cached: Type.Boolean(), cachedSelection: Type.Boolean() }),
        400: ErrorEnvelope,
        404: ErrorEnvelope,
      },
    },
  }, async (request, reply) => {
    const parsed = parseAnalyticsQuery(request.query as AnalyticsQueryInput, true);
    const loaded = await loadSetOrError(parsed);
    if ("error" in loaded) return reply.code(400).send({ error: loaded.error });
    const { params, setups } = loaded;
    if (setups.length > MAX_SETUPS_PER_COMPUTE) {
      return reply.code(400).send({ error: `set is too large for one computation (${setups.length} setups)` });
    }
    const inScope = filterSetupsByTime(setups, params.timeFilter);
    const selection = await selectSetupRows(db, setups, params);
    const scope = await computeScope(reader, setups, params);
    const scopeKey = matrixScopeKey(scope);
    const cached = setups.length > 0
      ? await getMatrixCell(db, scopeKey, params.compute, params.cacheTimeKey).catch(() => undefined)
      : undefined;
    if (cached !== undefined) {
      return { ...cached, cached: true, cachedSelection: selection.cachedSelection };
    }
    const { evaluations, eligibleCount, filteredCount } = await evaluateRows(reader, selection.rows, params);
    const payload = aggregatePayload(params, selection, inScope.length, eligibleCount, filteredCount, evaluations);
    if (setups.length > 0) {
      await saveMatrixCell(db, scopeKey, params.compute, params.cacheTimeKey, payload).catch(() => undefined);
    }
    return { ...payload, cached: false, cachedSelection: selection.cachedSelection };
  });

  app.get("/v1/analytics/trades", {
    preHandler: [requireApiKey],
    schema: {
      querystring: Type.Object(AnalyticsQuery),
      response: {
        200: Type.Object({ items: Type.Array(TradeRowDto), cachedCount: Type.Integer() }),
        400: ErrorEnvelope,
        404: ErrorEnvelope,
      },
    },
  }, async (request, reply) => {
    const parsed = parseAnalyticsQuery(request.query as AnalyticsQueryInput, true);
    const loaded = await loadSetOrError(parsed);
    if ("error" in loaded) return reply.code(400).send({ error: loaded.error });
    const { params, setups } = loaded;
    const scope = await computeScope(reader, setups, params);
    const rows = await listTradeRows(db, matrixScopeKey(scope), params.compute).catch(() => []);
    const setupsByIdentity = new Map(setups.map((setup) => [identityOf(setup), setup]));
    const items = rows.flatMap((row) => {
      const setup = setupsByIdentity.get(identityOf({ ...row.identity }));
      if (setup === undefined) return [];
      return [{
        setup: {
          exchange: setup.exchangeCode, category: setup.category, symbol: setup.symbol,
          openTimeUnix: setup.openTimeUnix, closeTimeUnix: setup.closeTimeUnix, closePrice: setup.closePrice,
        },
        direction: params.direction,
        tpPct: params.tpPct,
        slPct: params.slPct,
        status: (row.payload.status as string) ?? "no_data",
        entryPrice: (row.payload.entryPrice as number | null) ?? null,
        tpPrice: (row.payload.tpPrice as number | null) ?? null,
        slPrice: (row.payload.slPrice as number | null) ?? null,
        closeTimeUnix: (row.payload.closeTimeUnix as number | null) ?? null,
        riskReturn: (row.payload.riskReturn as number | null) ?? null,
      }];
    });
    return { items, cachedCount: items.length };
  });

  app.get("/v1/analytics/trades/:symbol/:closeTimeUnix", {
    preHandler: [requireApiKey],
    schema: {
      params: Type.Object({ symbol: Type.String({ minLength: 1, maxLength: 64 }), closeTimeUnix: Type.Integer({ minimum: 0 }) }),
      querystring: Type.Object({
        ...AnalyticsQuery,
        exchange: Type.Optional(Type.String({ minLength: 1, maxLength: 32 })),
        category: Type.Optional(Type.String({ minLength: 1, maxLength: 32 })),
      }),
      response: {
        200: Type.Object({
          setup: SetupDto,
          direction: Type.Union([Type.Literal("short"), Type.Literal("long")]),
          tpPct: Type.Number(),
          slPct: Type.Number(),
          status: Type.String(),
          entryPrice: NullableNumber,
          tpPrice: NullableNumber,
          slPrice: NullableNumber,
          closeTimeUnix: NullableNumber,
          riskReturn: NullableNumber,
          cached: Type.Boolean(),
        }),
        400: ErrorEnvelope,
        404: ErrorEnvelope,
      },
    },
  }, async (request, reply) => {
    const routeParams = request.params as { symbol: string; closeTimeUnix: number };
    const raw = request.query as AnalyticsQueryInput & { exchange?: string; category?: string };
    const parsed = parseAnalyticsQuery(raw, true);
    const loaded = await loadSetOrError(parsed);
    if ("error" in loaded) return reply.code(400).send({ error: loaded.error });
    const { params, setups } = loaded;
    const setup = setups.find(
      (candidate) =>
        candidate.symbol === routeParams.symbol &&
        candidate.closeTimeUnix === routeParams.closeTimeUnix &&
        (raw.exchange === undefined || candidate.exchangeCode === raw.exchange) &&
        (raw.category === undefined || candidate.category === raw.category),
    );
    if (setup === undefined) return reply.code(404).send({ error: "setup not found in set" });
    const setupDto = {
      exchange: setup.exchangeCode, category: setup.category, symbol: setup.symbol,
      openTimeUnix: setup.openTimeUnix, closeTimeUnix: setup.closeTimeUnix, closePrice: setup.closePrice,
    };
    const scope = await computeScope(reader, setups, params);
    const scopeKey = matrixScopeKey(scope);
    const cached = setups.length > 0
      ? await getTradeRow(db, scopeKey, params.compute, setup).catch(() => undefined)
      : undefined;
    if (cached !== undefined) {
      return {
        setup: setupDto,
        direction: params.direction,
        tpPct: params.tpPct,
        slPct: params.slPct,
        ...(cached as Record<string, unknown>),
        cached: true,
      };
    }
    const instrumentId = await reader.instrumentId(setup).catch(() => null);
    let result: Record<string, unknown>;
    if (instrumentId === null) {
      result = { status: "no_data", entryPrice: null, tpPrice: null, slPrice: null, closeTimeUnix: null, riskReturn: null };
    } else {
      const window = await reader
        .readWindow(instrumentId, setup.openTimeUnix, setup.openTimeUnix + params.timeShiftMinutes * 60 + 60 + params.horizonMinutes * 60)
        .catch(() => [] as OhlcCandle[]);
      const entry = resolveEntry(window, setup, params.timeShiftMinutes);
      if (entry === null) {
        result = { status: "no_data", entryPrice: null, tpPrice: null, slPrice: null, closeTimeUnix: null, riskReturn: null };
      } else if (
        params.priceChangePct !== null &&
        !passesPriceChange(Number(setup.closePrice), entry.entryPrice, params.direction, params.priceChangePct)
      ) {
        result = { status: "price_change_filtered", entryPrice: entry.entryPrice, tpPrice: null, slPrice: null, closeTimeUnix: null, riskReturn: null };
      } else {
        const evaluation = evaluateLimit({
          candles: window,
          setup,
          direction: params.direction,
          tpPct: params.tpPct,
          slPct: params.slPct,
          timeShiftMinutes: params.timeShiftMinutes,
          horizonMinutes: params.horizonMinutes,
        });
        result = {
          status: evaluation.status,
          entryPrice: evaluation.entryPrice,
          tpPrice: evaluation.tpPrice,
          slPrice: evaluation.slPrice,
          closeTimeUnix: evaluation.closeTimeUnix,
          riskReturn: riskReturn(evaluation.status, params.tpPct, params.slPct),
        };
      }
    }
    const payload: Record<string, unknown> = {
      status: result.status,
      entryPrice: result.entryPrice,
      tpPrice: result.tpPrice,
      slPrice: result.slPrice,
      closeTimeUnix: result.closeTimeUnix,
      riskReturn: result.riskReturn,
    };
    if (setups.length > 0) {
      await saveTradeRow(db, scopeKey, params.compute, setup, payload).catch(() => undefined);
    }
    return {
      setup: setupDto,
      direction: params.direction,
      tpPct: params.tpPct,
      slPct: params.slPct,
      ...payload,
      cached: false,
    };
  });

  app.get("/v1/analytics/cache/summary", {
    preHandler: [requireApiKey],
    schema: {
      response: {
        200: Type.Object({
          tables: Type.Array(Type.Object({ name: Type.String(), count: Type.Integer() })),
          total: Type.Integer(),
        }),
      },
    },
  }, async () => analyticsCacheSummary(db));

  app.delete("/v1/analytics/cache", {
    preHandler: [requireApiKey],
    schema: {
      response: {
        200: Type.Object({
          deleted: Type.Array(Type.Object({ name: Type.String(), count: Type.Integer() })),
          total: Type.Integer(),
        }),
      },
    },
  }, async () => clearAnalyticsCache(db));
}
