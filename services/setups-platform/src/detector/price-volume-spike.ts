import {
  addDecimals,
  compareScaled,
  mulByBigInt,
  parseDecimal,
  subDecimals,
  ZERO_DECIMAL,
  type Decimal,
} from "./decimal.js";

export const SPIKE_ALGORITHM_ID = "price_volume_spike";
export const SPIKE_ALGORITHM_VERSION = "1.0.0";

export interface SpikeParams {
  priceGrowthPct: string;
  windowMinutes: number;
  baselineLongDays: number;
  baselineShortDays: number;
  volumeMultiplier: string;
  cooldownMinutes: number;
}

export const DEFAULT_SPIKE_PARAMS: SpikeParams = {
  priceGrowthPct: "12",
  windowMinutes: 5,
  baselineLongDays: 30,
  baselineShortDays: 7,
  volumeMultiplier: "3",
  cooldownMinutes: 5,
};

export interface DetectorBar {
  openTimeUnix: number;
  close: string;
  /** Quote turnover in USDT for the minute. */
  quoteTurnover: string;
}

export interface SpikeSetup {
  algorithmId: typeof SPIKE_ALGORITHM_ID;
  algorithmVersion: string;
  openTimeUnix: number;
  closeTimeUnix: number;
  closePrice: string;
  params: SpikeParams;
}

export interface PushResult {
  status: "warmup" | "evaluated";
  /** Length of the current gapless minute series including this bar. */
  continuousMinutes: number;
  setup?: SpikeSetup | undefined;
}

const MINUTE_SECONDS = 60;

/**
 * Neutral `price_volume_spike` detector. The same instance semantics are used
 * by live and replay: gapless closed minutes only, warmup of
 * baselineLongDays*1440 consecutive minutes (including the evaluated minute),
 * baselines include the current window, and cooldown is measured between
 * signal minutes without requiring the condition to reset.
 */
export class PriceVolumeSpikeDetector {
  private readonly requiredMinutes: number;
  private readonly shortMinutes: number;
  private readonly windowMinutes: number;
  private readonly cooldownSeconds: number;
  private readonly growthDenominator = 100n;
  private readonly growthNumerator: bigint;
  private readonly multiplier: bigint;

  // TODO: replace array shift-based eviction with a fixed ring buffer when
  // instrument count grows; the array is already bounded by requiredMinutes.
  private volumes: Decimal[] = [];
  private sumWindow: Decimal = ZERO_DECIMAL;
  private sumShort: Decimal = ZERO_DECIMAL;
  private sumLong: Decimal = ZERO_DECIMAL;
  private closes: { time: number; close: Decimal }[] = [];
  private prevTime: number | null = null;
  private lastSetupTime: number | null = null;

  constructor(public readonly params: SpikeParams = DEFAULT_SPIKE_PARAMS) {
    this.requiredMinutes = params.baselineLongDays * 1440;
    this.shortMinutes = params.baselineShortDays * 1440;
    this.windowMinutes = params.windowMinutes;
    this.cooldownSeconds = params.cooldownMinutes * MINUTE_SECONDS;
    const pct = parseDecimal(params.priceGrowthPct);
    const hundred = { int: 100n, scale: 0 };
    const growth = addDecimals(hundred, pct);
    this.growthNumerator = growth.int * 10n ** BigInt(growth.scale);
    const multiplier = parseDecimal(params.volumeMultiplier);
    this.multiplier = multiplier.int * 10n ** BigInt(multiplier.scale);
  }

  push(bar: DetectorBar): PushResult {
    if (this.prevTime !== null && bar.openTimeUnix - this.prevTime !== MINUTE_SECONDS) {
      this.resetSeries();
    }
    this.prevTime = bar.openTimeUnix;

    const volume = parseDecimal(bar.quoteTurnover);
    this.volumes.push(volume);
    this.sumWindow = addDecimals(this.sumWindow, volume);
    this.sumShort = addDecimals(this.sumShort, volume);
    this.sumLong = addDecimals(this.sumLong, volume);

    if (this.volumes.length > this.requiredMinutes) {
      const leaving = this.volumes.shift();
      if (leaving !== undefined) {
        this.sumLong = subDecimals(this.sumLong, leaving);
      }
    }
    if (this.volumes.length > this.shortMinutes) {
      const leaving = this.volumes[this.volumes.length - 1 - this.shortMinutes];
      if (leaving !== undefined) {
        this.sumShort = subDecimals(this.sumShort, leaving);
      }
    }
    if (this.volumes.length > this.windowMinutes) {
      const leaving = this.volumes[this.volumes.length - 1 - this.windowMinutes];
      if (leaving !== undefined) {
        this.sumWindow = subDecimals(this.sumWindow, leaving);
      }
    }

    this.closes.push({ time: bar.openTimeUnix, close: parseDecimal(bar.close) });
    if (this.closes.length > this.windowMinutes + 1) {
      this.closes.shift();
    }

    if (this.volumes.length < this.requiredMinutes) {
      return { status: "warmup", continuousMinutes: this.volumes.length };
    }

    const setup = this.evaluate(bar);
    return { status: "evaluated", continuousMinutes: this.volumes.length, setup };
  }

  private evaluate(bar: DetectorBar): SpikeSetup | undefined {
    const previous = this.closes[0];
    const current = this.closes[this.closes.length - 1];
    if (
      previous === undefined ||
      current === undefined ||
      previous.time !== bar.openTimeUnix - this.windowMinutes * MINUTE_SECONDS
    ) {
      return undefined;
    }

    // Price condition: close(t) * 100 >= close(t-W) * (100 + growthPct).
    const priceGrowth = compareScaled(
      current.close,
      this.growthDenominator,
      previous.close,
      this.growthNumerator,
    );
    if (priceGrowth < 0) {
      return undefined;
    }

    // Volume condition (strict): sumWindow > multiplier * average window
    // volume over a baseline. avg_long = sumLong * W / L, so the comparison is
    // sumWindow * L > multiplier * sumLong * W. Baselines include the current
    // window minutes.
    const windowFactor = BigInt(this.windowMinutes);
    const longFactor = BigInt(this.requiredMinutes);
    const shortFactor = BigInt(this.shortMinutes);

    const overLong =
      compareScaled(
        this.sumWindow,
        longFactor,
        mulByBigInt(this.sumLong, this.multiplier * windowFactor),
        1n,
      ) > 0;
    const overShort =
      compareScaled(
        this.sumWindow,
        shortFactor,
        mulByBigInt(this.sumShort, this.multiplier * windowFactor),
        1n,
      ) > 0;
    if (!overLong && !overShort) {
      return undefined;
    }

    if (
      this.lastSetupTime !== null &&
      bar.openTimeUnix - this.lastSetupTime < this.cooldownSeconds
    ) {
      return undefined;
    }
    this.lastSetupTime = bar.openTimeUnix;

    return {
      algorithmId: SPIKE_ALGORITHM_ID,
      algorithmVersion: SPIKE_ALGORITHM_VERSION,
      openTimeUnix: bar.openTimeUnix,
      closeTimeUnix: bar.openTimeUnix + MINUTE_SECONDS,
      closePrice: bar.close,
      params: this.params,
    };
  }

  private resetSeries(): void {
    this.volumes = [];
    this.sumWindow = ZERO_DECIMAL;
    this.sumShort = ZERO_DECIMAL;
    this.sumLong = ZERO_DECIMAL;
    this.closes = [];
    this.lastSetupTime = null;
  }
}
