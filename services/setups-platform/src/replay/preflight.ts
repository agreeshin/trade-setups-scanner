import { sql, type Kysely } from "kysely";
import type { SignalsDB } from "../db/schema.js";
import type { SpikeParams } from "../detector/price-volume-spike.js";
import { getAlgorithm, validateSpikeParams } from "../algorithms.js";
import { fetchCatalog } from "../platform-catalog.js";
import { canonicalCacheKey } from "./cache.js";
import type { ReplayQuality } from "./quality.js";

export interface ReplayRequest {
  instruments: { exchange: string; category: string; symbol: string }[];
  fromUnix: number;
  toUnix: number;
  algorithmId?: string;
  algorithmVersion?: string;
  params?: Partial<SpikeParams>;
  async?: boolean;
}

export async function prepareReplayRequest(db: Kysely<SignalsDB>, body: ReplayRequest) {
  const algorithm = await getAlgorithm(db, body.algorithmId);
  if (!algorithm) return { error: "unknown_algorithm" } as const;
  if (body.algorithmVersion !== undefined && body.algorithmVersion !== algorithm.version) {
    return { error: "algorithm_version_changed_repeat_preflight" } as const;
  }
  const params = { ...algorithm.params, ...body.params };
  const error = validateSpikeParams(params);
  if (error || !Number.isSafeInteger(body.fromUnix) || !Number.isSafeInteger(body.toUnix) ||
    body.fromUnix < 0 || body.toUnix > 253_402_300_740 || body.fromUnix >= body.toUnix ||
    body.toUnix - body.fromUnix > 366 * 86400) {
    return { error: error ?? "invalid_range_max_366_days" } as const;
  }
  return { request: { ...body, algorithmId: algorithm.id, algorithmVersion: algorithm.version, params } };
}

export interface CoverageRange {
  instrumentId: number;
  fromUnix: number;
  toUnix: number;
  splitUnix: number;
  expectedMinutes: number;
  presentMinutes: number;
  missingMinutes: number;
  warmupExpectedMinutes: number;
  warmupPresentMinutes: number;
  signalExpectedMinutes: number;
  signalPresentMinutes: number;
  gaps: { fromUnix: number; toUnix: number }[];
  gapCount: number;
  offset: number;
  limit: number;
  checkedAt: string;
}

interface PreflightItem {
  exchange: string;
  category: string;
  symbol: string;
  cacheHit: boolean;
  status: "cached" | "ready" | "unavailable" | "not_found";
  computedAt?: string;
  quality?: ReplayQuality | null;
  setupCount?: number;
  coverage?: CoverageRange;
  error?: string;
}

export async function replayPreflight(db: Kysely<SignalsDB>,
  request: ReplayRequest & { algorithmId: string; algorithmVersion: string; params: SpikeParams },
  marketData: { url: string; apiKey: string }, signal: AbortSignal) {
  const requiredHistoryMinutes = request.params.baselineLongDays * 1440;
  const keys = request.instruments.map((instrument) => canonicalCacheKey({ ...instrument,
    exchangeCode: instrument.exchange, algorithmId: request.algorithmId, algorithmVersion: request.algorithmVersion,
    params: request.params, fromUnix: request.fromUnix, toUnix: request.toUnix }));
  // Metadata only: never load cached setup arrays for a preview. Request schema bounds this to 100 keys.
  const cached = await db.selectFrom("replay_cache").select(["cache_key", "computed_at", "quality"])
    .select(sql<number>`jsonb_array_length(setups)`.as("setup_count")).where("cache_key", "in", keys).execute();
  const byKey = new Map(cached.map((row) => [row.cache_key, row]));
  const results: PreflightItem[] = request.instruments.map((instrument, index) => {
    const hit = byKey.get(keys[index]!);
    return hit ? { ...instrument, cacheHit: true, status: "cached", computedAt: hit.computed_at.toISOString(),
      quality: hit.quality, setupCount: hit.setup_count }
      : { ...instrument, cacheHit: false, status: "unavailable", error: "market_data_unavailable" };
  });
  const misses = results.filter((item) => !item.cacheHit);
  if (misses.length && !signal.aborted) {
    try {
      const catalog = await fetchCatalog(marketData.url, marketData.apiKey, signal);
      const identities = new Map(catalog.map((item) => [JSON.stringify([item.exchange, item.category, item.symbol]), item.id]));
      const fromUnix = request.fromUnix - requiredHistoryMinutes * 60;
      const toUnix = request.toUnix;
      let cursor = 0;
      // At most four history requests and one bounded result array per HTTP operation.
      await Promise.all(Array.from({ length: Math.min(4, misses.length) }, async () => {
        while (!signal.aborted) {
          const item = misses[cursor++];
          if (!item) break;
          const id = identities.get(JSON.stringify([item.exchange, item.category, item.symbol]));
          if (id === undefined) { item.status = "not_found"; item.error = "instrument_not_found"; continue; }
          try {
            const query = new URLSearchParams({ fromUnix: String(fromUnix), toUnix: String(toUnix),
              splitUnix: String(request.fromUnix), offset: "0", limit: "50" });
            const response = await fetch(`${marketData.url}/v1/instruments/${id}/coverage-range?${query.toString()}`, {
              headers: { "x-api-key": marketData.apiKey }, signal: AbortSignal.any([signal, AbortSignal.timeout(15000)]),
            });
            if (response.status === 404) { item.status = "not_found"; item.error = "instrument_not_found"; continue; }
            if (!response.ok) continue;
            const coverage = await response.json() as CoverageRange;
            if (coverage.instrumentId !== id || coverage.fromUnix !== Math.ceil(fromUnix / 60) * 60 ||
              coverage.toUnix !== Math.ceil(toUnix / 60) * 60 || coverage.splitUnix !== Math.ceil(request.fromUnix / 60) * 60 ||
              !Array.isArray(coverage.gaps) || coverage.gaps.length > 50 ||
              ![coverage.expectedMinutes, coverage.presentMinutes, coverage.missingMinutes,
                coverage.warmupExpectedMinutes, coverage.warmupPresentMinutes,
                coverage.signalExpectedMinutes, coverage.signalPresentMinutes, coverage.gapCount]
                .every((value) => Number.isSafeInteger(value) && value >= 0) ||
              coverage.expectedMinutes !== (coverage.toUnix - coverage.fromUnix) / 60 ||
              coverage.missingMinutes !== coverage.expectedMinutes - coverage.presentMinutes ||
              coverage.warmupExpectedMinutes !== (coverage.splitUnix - coverage.fromUnix) / 60 ||
              coverage.signalExpectedMinutes !== (coverage.toUnix - coverage.splitUnix) / 60 ||
              coverage.warmupPresentMinutes > coverage.warmupExpectedMinutes ||
              coverage.signalPresentMinutes > coverage.signalExpectedMinutes ||
              coverage.presentMinutes !== coverage.warmupPresentMinutes + coverage.signalPresentMinutes ||
              coverage.offset !== 0 || coverage.limit !== 50 || coverage.gaps.length !== Math.min(50, coverage.gapCount) ||
              (coverage.gapCount === 0) !== (coverage.missingMinutes === 0) ||
              typeof coverage.checkedAt !== "string" || !Number.isFinite(Date.parse(coverage.checkedAt)) ||
              coverage.gaps.some((gap, index) => !Number.isSafeInteger(gap.fromUnix) || !Number.isSafeInteger(gap.toUnix) ||
                gap.fromUnix % 60 !== 0 || gap.toUnix % 60 !== 0 || gap.fromUnix < coverage.fromUnix ||
                gap.toUnix > coverage.toUnix || gap.fromUnix >= gap.toUnix ||
                (index > 0 && gap.fromUnix < coverage.gaps[index - 1]!.toUnix))) {
              item.error = "invalid_coverage_response"; continue;
            }
            item.status = "ready";
            item.coverage = coverage;
            delete item.error;
          } catch { /* This instrument remains unavailable, not empty or missing. */ }
        }
      }));
    } catch { /* Cache hits remain usable when the history catalog is unavailable. */ }
  }
  return { checkedAt: new Date().toISOString(), algorithmVersion: request.algorithmVersion, requiredHistoryMinutes,
    canRun: results.every((item) => item.status === "cached" || item.status === "ready"),
    request: { ...request, async: true }, results };
}
