import { writeFileSync } from "node:fs";
import { createHmac } from "node:crypto";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  PriceVolumeSpikeDetector,
  DEFAULT_SPIKE_PARAMS,
  type SpikeParams,
  type PushResult,
} from "../../setups-platform/src/detector/price-volume-spike.js";
import { ReplayQualityTracker } from "../../setups-platform/src/replay/quality.js";
import { replayRun } from "../../setups-platform/src/replay/replay-runner.js";
import { ArrayCandleSource } from "../../setups-platform/src/replay/source.js";
import { canonicalCacheKey } from "../../setups-platform/src/replay/cache.js";
import {
  selectByMaturity,
  setupMonths,
  setupClustersCount,
  filterByMonth,
  filterByDayRange,
  evaluateLimit,
  aggregateLimitOutcomes,
  passesPriceChange,
  riskReturn,
  DEFAULT_CLUSTER_GAP_HOURS,
  type AnalyticsSetup,
  type OhlcCandle,
  type TradeDirection,
  type LimitEvaluation,
} from "../../setups-platform/src/analytics/engine.js";

type BarSpec = { t: number; close: string; turnover: string };
type SpikeEvent = {
  at: number;
  dur: number;
  closes: string[];
  refClose: string;
  turnover: string;
  turnoverLeadBars: number;
};
type GapEvent = { at: number; minutes: number };
type ScenarioSpec = {
  name: string;
  params: SpikeParams;
  count: number;
  baseTurnover: string;
  gaps: GapEvent[];
  spikes: SpikeEvent[];
  trackFromIndex: number;
  trackToIndex: number;
  lastSetupUnix?: number;
};

const T0 = 1_700_000_000;
const MINUTE = 60;

function makeLcg(seed: bigint): () => number {
  let s = seed;
  return () => {
    s = (6364136223846793005n * s + 1442695040888963407n) & 0xffff_ffff_ffff_ffffn;
    return Number(s >> 11n) / 2 ** 53;
  };
}

function steppedCloses(steps: string[], dur: number, stepBars: number): string[] {
  return Array.from({ length: dur }, (_, k) => steps[Math.min(Math.floor(k / stepBars), steps.length - 1)]);
}

function buildBars(spec: ScenarioSpec): BarSpec[] {
  const rand = makeLcg(0x5eed1234n);
  const closes: string[] = [];
  const turnovers: string[] = [];
  let close = 100;
  for (let i = 0; i < spec.count; i += 1) {
    close = Math.max(1, close * (1 + (rand() - 0.5) * 0.002));
    closes.push(close.toFixed(8));
    turnovers.push(spec.baseTurnover);
  }
  for (const spike of spec.spikes) {
    for (let k = 0; k < 5; k += 1) {
      closes[spike.at - 5 + k] = spike.refClose;
    }
    for (let k = 0; k < spike.turnoverLeadBars && spike.at - 5 + k < spec.count; k += 1) {
      turnovers[spike.at - 5 + k] = spike.turnover;
    }
    for (let k = 0; k < spike.dur && spike.at + k < spec.count; k += 1) {
      closes[spike.at + k] = spike.closes[k];
      turnovers[spike.at + k] = spike.turnover;
    }
  }
  const bars: BarSpec[] = [];
  let t = T0;
  const gapAt = new Map<number, number>();
  for (const gap of spec.gaps) gapAt.set(gap.at, gap.minutes);
  for (let i = 0; i < spec.count; i += 1) {
    const gap = gapAt.get(i);
    if (gap !== undefined) t += gap * MINUTE;
    bars.push({ t, close: closes[i], turnover: turnovers[i] });
    t += MINUTE;
  }
  return bars;
}

function isInteresting(push: PushResult): boolean {
  if (push.setup !== undefined) return true;
  if (push.status === "evaluated") return true;
  return push.skipReason !== undefined && push.skipReason !== "insufficient_warmup";
}

function runScenario(spec: ScenarioSpec) {
  const bars = buildBars(spec);
  const detector = new PriceVolumeSpikeDetector(spec.params, spec.lastSetupUnix ?? null);
  const trackFromUnix = bars[spec.trackFromIndex].t;
  const trackToUnix = bars[spec.trackToIndex - 1].t + MINUTE;
  const tracker = new ReplayQualityTracker(
    trackFromUnix,
    trackToUnix,
    spec.params.baselineLongDays * 1440,
  );
  const interesting: PushResult[] = [];
  let warmupCount = 0;
  let evaluatedCount = 0;
  let setupCount = 0;
  for (const bar of bars) {
    const push = detector.push({ openTimeUnix: bar.t, close: bar.close, quoteTurnover: bar.turnover });
    tracker.observe(bar.t, push);
    if (push.status === "warmup") warmupCount += 1;
    else evaluatedCount += 1;
    if (push.setup !== undefined) setupCount += 1;
    if (isInteresting(push)) interesting.push(push);
  }
  tracker.finish();
  return {
    name: spec.name,
    params: spec.params,
    ...(spec.lastSetupUnix !== undefined ? { lastSetupUnix: spec.lastSetupUnix } : {}),
    trackFromUnix,
    trackToUnix,
    bars: bars.map((bar) => [bar.t, bar.close, bar.turnover]),
    digest: {
      pushCount: bars.length,
      warmupCount,
      evaluatedCount,
      setupCount,
      interestingCount: interesting.length,
    },
    pushes: interesting,
    quality: tracker.quality,
    intervals: tracker.drain(),
  };
}

const smallWindows: SpikeParams = {
  ...DEFAULT_SPIKE_PARAMS,
  baselineLongDays: 2,
  baselineShortDays: 1,
};
const oneDay: SpikeParams = {
  ...DEFAULT_SPIKE_PARAMS,
  baselineLongDays: 1,
  baselineShortDays: 1,
};
const t = (index: number) => T0 + index * MINUTE;

const SPECS: ScenarioSpec[] = [
  {
    name: "cooldown_repeats",
    params: smallWindows,
    count: 2990,
    baseTurnover: "1000000.00000000",
    gaps: [{ at: 2000, minutes: 2 }],
    spikes: [
      {
        at: 2950,
        dur: 12,
        closes: steppedCloses(["113.00000000", "127.69000000", "144.28970000"], 12, 5),
        refClose: "100.00000000",
        turnover: "10000000.00000000",
        turnoverLeadBars: 5,
      },
    ],
    trackFromIndex: 2880,
    trackToIndex: 2990,
  },
  {
    name: "price_exact_boundary",
    params: oneDay,
    count: 1460,
    baseTurnover: "95",
    gaps: [],
    spikes: [
      {
        at: 1455,
        dur: 5,
        closes: ["112.00000000", "112.00000000", "112.00000000", "112.00000000", "112.00000000"],
        refClose: "100.00000000",
        turnover: "289",
        turnoverLeadBars: 5,
      },
    ],
    trackFromIndex: 1440,
    trackToIndex: 1460,
  },
  {
    name: "price_below_boundary",
    params: oneDay,
    count: 1460,
    baseTurnover: "95",
    gaps: [],
    spikes: [
      {
        at: 1455,
        dur: 5,
        closes: ["111.99999999", "111.99999999", "111.99999999", "111.99999999", "111.99999999"],
        refClose: "100.00000000",
        turnover: "289",
        turnoverLeadBars: 5,
      },
    ],
    trackFromIndex: 1440,
    trackToIndex: 1460,
  },
  {
    name: "volume_below_boundary",
    params: oneDay,
    count: 1460,
    baseTurnover: "95",
    gaps: [],
    spikes: [
      {
        at: 1455,
        dur: 5,
        closes: ["113.00000000", "113.00000000", "113.00000000", "113.00000000", "113.00000000"],
        refClose: "100.00000000",
        turnover: "287",
        turnoverLeadBars: 5,
      },
    ],
    trackFromIndex: 1440,
    trackToIndex: 1460,
  },
  {
    name: "volume_above_boundary",
    params: oneDay,
    count: 1460,
    baseTurnover: "95",
    gaps: [],
    spikes: [
      {
        at: 1455,
        dur: 5,
        closes: ["113.00000000", "113.00000000", "113.00000000", "113.00000000", "113.00000000"],
        refClose: "100.00000000",
        turnover: "288",
        turnoverLeadBars: 5,
      },
    ],
    trackFromIndex: 1440,
    trackToIndex: 1460,
  },
  {
    name: "gaps_reset",
    params: { ...smallWindows, maxConsecutiveGapMinutes: 3 },
    count: 2990,
    baseTurnover: "1000000.00000000",
    gaps: [
      { at: 1500, minutes: 2 },
      { at: 1600, minutes: 2 },
      { at: 2400, minutes: 10 },
    ],
    spikes: [],
    trackFromIndex: 2880,
    trackToIndex: 2990,
  },
  {
    name: "gap_tolerance",
    params: { ...oneDay, maxGapPercent: "0.2" },
    count: 3045,
    baseTurnover: "1000000.00000000",
    gaps: [
      { at: 1500, minutes: 2 },
      { at: 1600, minutes: 1 },
    ],
    spikes: [],
    trackFromIndex: 1440,
    trackToIndex: 3045,
  },
  {
    name: "last_setup_resume",
    params: oneDay,
    count: 1500,
    baseTurnover: "1000000.00000000",
    gaps: [],
    spikes: [
      {
        at: 1470,
        dur: 6,
        closes: steppedCloses(["113.00000000", "127.69000000"], 6, 5),
        refClose: "100.00000000",
        turnover: "10000000.00000000",
        turnoverLeadBars: 5,
      },
    ],
    trackFromIndex: 1440,
    trackToIndex: 1500,
    lastSetupUnix: t(1470),
  },
  {
    name: "defaults_warmup",
    params: DEFAULT_SPIKE_PARAMS,
    count: 120,
    baseTurnover: "1000000.00000000",
    gaps: [],
    spikes: [],
    trackFromIndex: 100,
    trackToIndex: 120,
  },
];

function buildAnalyticsSection() {
  const boundary = 1_701_384_000;
  const setup = (symbol: string, closeTimeUnix: number): AnalyticsSetup => ({
    exchangeCode: "bybit",
    category: "linear",
    symbol,
    openTimeUnix: closeTimeUnix - MINUTE,
    closeTimeUnix,
    closePrice: "100.00000000",
  });
  const setups: AnalyticsSetup[] = [
    setup("BTCUSDT", boundary),
    setup("BTCUSDT", boundary + 3600),
    setup("BTCUSDT", boundary + 11 * 3600),
    setup("BTCUSDT", boundary + 11 * 3600 + 1800),
    setup("BTCUSDT", boundary + 11 * 3600 + 3600),
    setup("ETHUSDT", boundary + 1800),
    setup("ETHUSDT", boundary + 3600),
    setup("ETHUSDT", boundary + 5400),
    setup("ETHUSDT", boundary + 7200),
    setup("ETHUSDT", boundary + 9000),
  ];
  const defaultGap = DEFAULT_CLUSTER_GAP_HOURS * 3600;
  const selections = [0, 0.3, 0.7, 1].flatMap((maturity) =>
    [defaultGap, 1800].map((maxGapSeconds) => ({
      maturity,
      maxGapSeconds,
      result: selectByMaturity(setups, maturity, maxGapSeconds),
    })),
  );
  const clustering = {
    setups,
    months: setupMonths(setups),
    clusterCounts: [
      { maxGapSeconds: defaultGap, count: setupClustersCount(setups, defaultGap) },
      { maxGapSeconds: 1800, count: setupClustersCount(setups, 1800) },
    ],
    selections,
    monthFilters: [
      { month: "2023-11", rows: filterByMonth(setups, "2023-11") },
      { month: "2023-10", rows: filterByMonth(setups, "2023-10") },
    ],
    dayRangeFilters: [
      { fromDay: "2023-11-30", toDay: "2023-12-01", rows: filterByDayRange(setups, "2023-11-30", "2023-12-01") },
      { fromDay: "2023-12-02", toDay: "2023-12-31", rows: filterByDayRange(setups, "2023-12-02", "2023-12-31") },
    ],
  };

  const TA = 1_701_384_000;
  const mkSetup = (symbol: string, openTimeUnix: number): AnalyticsSetup => ({
    exchangeCode: "bybit",
    category: "linear",
    symbol,
    openTimeUnix,
    closeTimeUnix: openTimeUnix + MINUTE,
    closePrice: "100.00000000",
  });
  const candle = (openTimeUnix: number, open: number, high: number, low: number, close: number): OhlcCandle => ({
    openTimeUnix,
    open,
    high,
    low,
    close,
  });
  const flatBars = (from: number, count: number, base: number): OhlcCandle[] =>
    Array.from({ length: count }, (_, i) => candle(from + i * MINUTE, base, base + 0.5, base - 0.5, base));

  const caseList: {
    name: string;
    candles: OhlcCandle[];
    setup: AnalyticsSetup;
    direction: TradeDirection;
    tpPct: number;
    slPct: number;
    timeShiftMinutes: number;
    horizonMinutes: number;
  }[] = [
    {
      name: "short_sl_hit",
      candles: [candle(TA, 100, 100.5, 99.5, 100), candle(TA + 60, 100, 120, 99, 110)],
      setup: mkSetup("AAAUSDT", TA),
      direction: "short",
      tpPct: 10,
      slPct: 10,
      timeShiftMinutes: 0,
      horizonMinutes: 60,
    },
    {
      name: "short_tp_hit",
      candles: [
        candle(TA, 100, 100.5, 99.5, 100),
        ...flatBars(TA + 60, 4, 99),
        candle(TA + 300, 99, 99.5, 89.5, 90),
      ],
      setup: mkSetup("BBBUSDT", TA),
      direction: "short",
      tpPct: 10,
      slPct: 10,
      timeShiftMinutes: 0,
      horizonMinutes: 60,
    },
    {
      name: "double_touch_prefers_sl",
      candles: [candle(TA, 100, 100.5, 99.5, 100), candle(TA + 60, 100, 115, 89, 100)],
      setup: mkSetup("CCCUSDT", TA),
      direction: "short",
      tpPct: 10,
      slPct: 10,
      timeShiftMinutes: 0,
      horizonMinutes: 60,
    },
    {
      name: "no_data_missing_entry",
      candles: [
        candle(TA, 100, 100.5, 99.5, 100),
        candle(TA + 60, 100, 100.5, 99.5, 100),
        candle(TA + 420, 100, 100.5, 99.5, 100),
      ],
      setup: mkSetup("DDDUSDT", TA),
      direction: "short",
      tpPct: 10,
      slPct: 10,
      timeShiftMinutes: 5,
      horizonMinutes: 60,
    },
    {
      name: "unclosed_far_levels",
      candles: flatBars(TA, 10, 100),
      setup: mkSetup("EEEUSDT", TA),
      direction: "short",
      tpPct: 30,
      slPct: 30,
      timeShiftMinutes: 0,
      horizonMinutes: 9,
    },
    {
      name: "long_tp_hit",
      candles: [
        candle(TA, 100, 100.5, 99.5, 100),
        ...flatBars(TA + 60, 2, 101),
        candle(TA + 180, 101, 111.5, 100.5, 111),
      ],
      setup: mkSetup("FFFUSDT", TA),
      direction: "long",
      tpPct: 10,
      slPct: 10,
      timeShiftMinutes: 0,
      horizonMinutes: 60,
    },
    {
      name: "long_sl_hit",
      candles: [candle(TA, 100, 100.5, 99.5, 100), candle(TA + 60, 100, 100.5, 89, 92)],
      setup: mkSetup("GGGUSDT", TA),
      direction: "long",
      tpPct: 10,
      slPct: 10,
      timeShiftMinutes: 0,
      horizonMinutes: 60,
    },
    {
      name: "shift_3_entry",
      candles: [
        candle(TA, 100, 100.5, 99.5, 100),
        candle(TA + 60, 100, 100.5, 99.5, 100),
        candle(TA + 120, 100, 100.5, 99.5, 100),
        candle(TA + 180, 104, 104.5, 103.5, 105),
        candle(TA + 240, 105, 116, 104, 110),
      ],
      setup: mkSetup("HHHUSDT", TA),
      direction: "short",
      tpPct: 10,
      slPct: 10,
      timeShiftMinutes: 3,
      horizonMinutes: 30,
    },
  ];
  const evaluations: LimitEvaluation[] = caseList.map((testCase) =>
    evaluateLimit(testCase),
  );
  const evaluation = {
    cases: caseList.map((testCase, index) => ({
      name: testCase.name,
      setup: testCase.setup,
      direction: testCase.direction,
      tpPct: testCase.tpPct,
      slPct: testCase.slPct,
      timeShiftMinutes: testCase.timeShiftMinutes,
      horizonMinutes: testCase.horizonMinutes,
      candles: testCase.candles,
      evaluation: evaluations[index],
    })),
    aggregate: aggregateLimitOutcomes(evaluations, 10),
    riskReturns: [
      riskReturn("tp", 2, 1),
      riskReturn("sl", 2, 1),
      riskReturn("unclosed", 2, 1),
      riskReturn("no_data", 2, 1),
    ],
  };

  const priceGate = {
    cases: (["short", "long"] as TradeDirection[]).flatMap((direction) =>
      [-5, 0, 3].flatMap((pct) =>
        [95, 100, 103].map((shifted) => ({
          direction,
          pct,
          shifted,
          setupClose: 100,
          passed: passesPriceChange(100, shifted, direction, pct),
        })),
      ),
    ),
  };

  return { clustering, evaluation, priceGate };
}

async function buildReplaySection() {
  const spec: ScenarioSpec = {
    name: "runner",
    params: oneDay,
    count: 1500,
    baseTurnover: "1000000.00000000",
    gaps: [{ at: 1450, minutes: 2 }],
    spikes: [
      {
        at: 1470,
        dur: 11,
        closes: steppedCloses(["113.00000000", "127.69000000", "144.28970000"], 11, 5),
        refClose: "100.00000000",
        turnover: "10000000.00000000",
        turnoverLeadBars: 5,
      },
    ],
    trackFromIndex: 1440,
    trackToIndex: 1500,
  };
  const bars = buildBars(spec).map((bar) => ({
    openTimeUnix: bar.t,
    close: bar.close,
    volume: "1.00000000",
    quoteTurnover: bar.turnover,
  }));
  const fromUnix = bars[1440].openTimeUnix;
  const toUnix = bars[bars.length - 1].openTimeUnix + MINUTE;
  const result = await replayRun({ source: new ArrayCandleSource(bars), fromUnix, toUnix, params: spec.params });
  return { params: spec.params, fromUnix, toUnix, bars, result };
}

const scenarios = SPECS.map(runScenario);
const analytics = buildAnalyticsSection();
const replay = await buildReplaySection();
const cacheKeyQueries = [
  {
    exchangeCode: "bybit",
    category: "linear",
    symbol: "BTCUSDT",
    algorithmId: "price_volume_spike",
    algorithmVersion: "1.1.1",
    params: DEFAULT_SPIKE_PARAMS,
    fromUnix: 1699920000,
    toUnix: 1700006400,
  },
  {
    exchangeCode: "bybit",
    category: "linear",
    symbol: "ETHUSDT",
    algorithmId: "price_volume_spike",
    algorithmVersion: "1.1.1",
    params: oneDay,
    fromUnix: 1699920000,
    toUnix: 1702512000,
  },
  {
    exchangeCode: "bybit",
    category: "spot",
    symbol: "SOLUSDT",
    algorithmId: "price_volume_spike",
    algorithmVersion: "1.1.1",
    params: { ...smallWindows, maxGapPercent: "0.5", volumeMultiplier: "2.5" },
    fromUnix: 1600000000,
    toUnix: 1600086400,
  },
  {
    exchangeCode: "bybit",
    category: "linear",
    symbol: "BTCUSDT",
    algorithmId: "price_volume_spike",
    algorithmVersion: "1.1.1",
    params: {
      windowMinutes: smallWindows.windowMinutes,
      baselineLongDays: smallWindows.baselineLongDays,
      baselineShortDays: smallWindows.baselineShortDays,
      volumeMultiplier: smallWindows.volumeMultiplier,
      cooldownMinutes: smallWindows.cooldownMinutes,
      maxGapPercent: smallWindows.maxGapPercent,
      maxConsecutiveGapMinutes: smallWindows.maxConsecutiveGapMinutes,
      requiredRecentMinutes: smallWindows.requiredRecentMinutes,
      priceGrowthPct: smallWindows.priceGrowthPct,
    },
    fromUnix: 1699920000,
    toUnix: 1700006400,
  },
];
const cacheKeys = {
  cases: cacheKeyQueries.map((query) => ({ query, key: canonicalCacheKey(query) })),
};
const authSignKey = Buffer.alloc(32, 7);
const auth = {
  signCases: [
    "1700000000000.0123456789abcdef0123456789abcdef",
    "9999999999999.ffffffffffffffffffffffffffffffff",
  ].map((payload) => ({
    payload,
    hmac: createHmac("sha256", authSignKey).update(payload).digest("hex"),
  })),
};
const fixture = {
  algorithmId: "price_volume_spike",
  algorithmVersion: "1.1.1",
  scenarios,
  analytics,
  replay,
  cacheKeys,
  auth,
};
const outPath = join(dirname(fileURLToPath(import.meta.url)), "parity.json");
writeFileSync(outPath, `${JSON.stringify(fixture, null, 1)}\n`);
for (const scenario of scenarios) {
  console.log(
    `${scenario.name}: bars=${scenario.digest.pushCount} setups=${scenario.digest.setupCount} interesting=${scenario.digest.interestingCount} state=${scenario.quality.state}`,
  );
}
console.log(`analytics: selections=${analytics.clustering.selections.length} evalCases=${analytics.evaluation.cases.length} gate=${analytics.priceGate.cases.length}`);
console.log(`replay runner: setups=${replay.result.setups.length} processed=${replay.result.processedMinutes} warmup=${replay.result.warmupMinutes}`);
console.log(`written: ${outPath}`);
