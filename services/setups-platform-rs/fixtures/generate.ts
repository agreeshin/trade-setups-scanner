import { writeFileSync } from "node:fs";
import { dirname, join } from "node:path";
import { fileURLToPath } from "node:url";
import {
  PriceVolumeSpikeDetector,
  DEFAULT_SPIKE_PARAMS,
  type SpikeParams,
  type PushResult,
} from "../../setups-platform/src/detector/price-volume-spike.js";
import { ReplayQualityTracker } from "../../setups-platform/src/replay/quality.js";

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

const scenarios = SPECS.map(runScenario);
const fixture = {
  algorithmId: "price_volume_spike",
  algorithmVersion: "1.1.1",
  scenarios,
};
const outPath = join(dirname(fileURLToPath(import.meta.url)), "parity.json");
writeFileSync(outPath, `${JSON.stringify(fixture, null, 1)}\n`);
for (const scenario of scenarios) {
  console.log(
    `${scenario.name}: bars=${scenario.digest.pushCount} setups=${scenario.digest.setupCount} interesting=${scenario.digest.interestingCount} state=${scenario.quality.state}`,
  );
}
console.log(`written: ${outPath}`);
