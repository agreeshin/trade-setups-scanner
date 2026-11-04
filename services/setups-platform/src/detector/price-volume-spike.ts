import {
  addDecimals,
  compareScaled,
  decimalRatio,
  mulByBigInt,
  parseDecimal,
  subDecimals,
  ZERO_DECIMAL,
  type Decimal,
} from "./decimal.js";

export const SPIKE_ALGORITHM_ID = "price_volume_spike";
export const SPIKE_ALGORITHM_VERSION = "1.1.1";

export interface SpikeParams {
  priceGrowthPct: string;
  windowMinutes: number;
  baselineLongDays: number;
  baselineShortDays: number;
  volumeMultiplier: string;
  cooldownMinutes: number;
  /** Max percentage of missing candles allowed in the warmup window, e.g. "1" = 1%. */
  maxGapPercent: string;
  /** Max consecutive missing minutes in a single gap. */
  maxConsecutiveGapMinutes: number;
  /** The last N minutes before an evaluated minute must have real candles. */
  requiredRecentMinutes: number;
}

export const DEFAULT_SPIKE_PARAMS: SpikeParams = {
  priceGrowthPct: "12",
  windowMinutes: 5,
  baselineLongDays: 30,
  baselineShortDays: 7,
  volumeMultiplier: "3",
  cooldownMinutes: 5,
  maxGapPercent: "1",
  maxConsecutiveGapMinutes: 5,
  requiredRecentMinutes: 10,
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
  details?: {
    growthPct: string | null;
    volumeLongRatio: string | null;
    volumeShortRatio: string | null;
    overLong: boolean;
    overShort: boolean;
    missingMinutes: number;
  };
}

export interface PushResult {
  status: "warmup" | "evaluated";
  /** Length of the current window (real + tolerated missing minutes). */
  continuousMinutes: number;
  setup?: SpikeSetup | undefined;
}

const MINUTE_SECONDS = 60;

interface CloseEntry {
  time: number;
  close: Decimal | null;
}

/**
 * Neutral `price_volume_spike` detector with gap tolerance.
 *
 * Missing candles are tolerated up to `maxGapPercent` of the total warmup
 * window and `maxConsecutiveGapMinutes` per single gap. A minute is only
 * evaluated when the last `requiredRecentMinutes` entries are real (non-gap).
 * Missing minutes contribute zero volume to all baselines.
 */
export class PriceVolumeSpikeDetector {
  private readonly requiredMinutes: number;
  private readonly shortMinutes: number;
  private readonly windowMinutes: number;
  private readonly cooldownSeconds: number;
  private readonly growthDenominator: bigint;
  private readonly growthNumerator: bigint;
  private readonly multiplier: bigint;
  private readonly multiplierDenominator: bigint;
  private readonly maxConsecutiveGapMinutes: number;
  private readonly requiredRecentMinutes: number;
  private readonly maxMissingCount: number;

  // volumes holds one entry per minute position; null = missing minute
  private volumes: (Decimal | null)[] = [];
  private missingInWindow = 0;
  private sumWindow: Decimal = ZERO_DECIMAL;
  private sumShort: Decimal = ZERO_DECIMAL;
  private sumLong: Decimal = ZERO_DECIMAL;
  private closes: CloseEntry[] = [];
  private prevTime: number | null = null;
  private lastSetupTime: number | null = null;

  constructor(public readonly params: SpikeParams = DEFAULT_SPIKE_PARAMS, lastSetupTime: number | null = null) {
    this.lastSetupTime = lastSetupTime;
    this.requiredMinutes = params.baselineLongDays * 1440;
    this.shortMinutes = params.baselineShortDays * 1440;
    this.windowMinutes = params.windowMinutes;
    this.cooldownSeconds = params.cooldownMinutes * MINUTE_SECONDS;
    this.maxConsecutiveGapMinutes = params.maxConsecutiveGapMinutes;
    this.requiredRecentMinutes = params.requiredRecentMinutes;

    // maxMissingCount = floor(requiredMinutes * maxGapPercent / 100)
    const gapPercent = parseDecimal(params.maxGapPercent);
    this.maxMissingCount = Number(BigInt(this.requiredMinutes) * gapPercent.int / (100n * 10n ** BigInt(gapPercent.scale)));

    const pct = parseDecimal(params.priceGrowthPct);
    const hundred = { int: 100n, scale: 0 };
    const growth = addDecimals(hundred, pct);
    this.growthNumerator = growth.int;
    this.growthDenominator = 100n * 10n ** BigInt(growth.scale);
    const multiplier = parseDecimal(params.volumeMultiplier);
    this.multiplier = multiplier.int;
    this.multiplierDenominator = 10n ** BigInt(multiplier.scale);
  }

  push(bar: DetectorBar): PushResult {
    if (this.prevTime !== null) {
      const gapMinutes = Math.floor(
        (bar.openTimeUnix - this.prevTime - MINUTE_SECONDS) / MINUTE_SECONDS,
      );
      if (gapMinutes > 0) {
        if (gapMinutes > this.maxConsecutiveGapMinutes) {
          this.resetSeries();
        } else {
          for (let i = 0; i < gapMinutes; i += 1) {
            this.pushEntry({
              time: this.prevTime + (i + 1) * MINUTE_SECONDS,
              volume: null,
              close: null,
            });
          }
          if (this.missingInWindow > this.maxMissingCount) {
            this.resetSeries();
          }
        }
      }
    }
    this.prevTime = bar.openTimeUnix;

    this.pushEntry({
      time: bar.openTimeUnix,
      volume: parseDecimal(bar.quoteTurnover),
      close: parseDecimal(bar.close),
    });

    if (this.volumes.length < this.requiredMinutes) {
      return { status: "warmup", continuousMinutes: this.volumes.length };
    }
    if (this.volumes.slice(-this.requiredRecentMinutes || this.volumes.length).some((volume) => volume === null)) {
      return { status: "warmup", continuousMinutes: this.volumes.length };
    }

    const setup = this.evaluate(bar);
    return { status: "evaluated", continuousMinutes: this.volumes.length, setup };
  }

  private pushEntry(entry: {
    time: number;
    volume: Decimal | null;
    close: Decimal | null;
  }): void {
    const vol = entry.volume ?? ZERO_DECIMAL;

    this.volumes.push(entry.volume);
    if (entry.volume === null) {
      this.missingInWindow += 1;
    }
    this.sumWindow = addDecimals(this.sumWindow, vol);
    this.sumShort = addDecimals(this.sumShort, vol);
    this.sumLong = addDecimals(this.sumLong, vol);

    // Evict from short window (position from the end)
    if (this.volumes.length > this.shortMinutes) {
      const leaving = this.volumes[this.volumes.length - 1 - this.shortMinutes];
      if (leaving !== undefined) {
        this.sumShort = subDecimals(this.sumShort, leaving ?? ZERO_DECIMAL);
      }
    }
    // Evict from price window
    if (this.volumes.length > this.windowMinutes) {
      const leaving = this.volumes[this.volumes.length - 1 - this.windowMinutes];
      if (leaving !== undefined) {
        this.sumWindow = subDecimals(this.sumWindow, leaving ?? ZERO_DECIMAL);
      }
    }

    // Retain the outgoing position until all rolling sums have consumed it.
    if (this.volumes.length > this.requiredMinutes) {
      const leaving = this.volumes.shift();
      if (leaving === null) this.missingInWindow -= 1;
      this.sumLong = subDecimals(this.sumLong, leaving ?? ZERO_DECIMAL);
    }

    this.closes.push({ time: entry.time, close: entry.close });
    if (this.closes.length > this.windowMinutes + 1) {
      this.closes.shift();
    }
  }

  private evaluate(bar: DetectorBar): SpikeSetup | undefined {
    // The last requiredRecentMinutes entries must be real (non-gap) candles.
    const recentStart = this.volumes.length - this.requiredRecentMinutes;
    for (let i = Math.max(recentStart, 0); i < this.volumes.length; i += 1) {
      if (this.volumes[i] === null) {
        return undefined;
      }
    }

    // Total missing must stay within threshold.
    if (this.missingInWindow > this.maxMissingCount) {
      return undefined;
    }

    // Price: find the close at exactly W minutes back.
    const targetTime = bar.openTimeUnix - this.windowMinutes * MINUTE_SECONDS;
    const previous = this.closes.find((c) => c.time === targetTime);
    if (previous === undefined || previous.close === null) {
      return undefined;
    }
    const current = this.closes[this.closes.length - 1];
    if (current === undefined || current.close === null) {
      return undefined;
    }

    // close(t) * 100 >= close(t-W) * (100 + growthPct)
    const priceGrowth = compareScaled(
      current.close,
      this.growthDenominator,
      previous.close,
      this.growthNumerator,
    );
    if (priceGrowth < 0) {
      return undefined;
    }

    // Volume condition (strict): sumWindow > multiplier * avg baseline volume.
    // Missing minutes contribute zero, so baselines are naturally diluted.
    const windowFactor = BigInt(this.windowMinutes);
    const longFactor = BigInt(this.requiredMinutes);
    const shortFactor = BigInt(this.shortMinutes);

    const overLong =
      compareScaled(
        this.sumWindow,
        longFactor * this.multiplierDenominator,
        mulByBigInt(this.sumLong, this.multiplier * windowFactor),
        1n,
      ) > 0;
    const overShort =
      compareScaled(
        this.sumWindow,
        shortFactor * this.multiplierDenominator,
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
      details: {
        growthPct: decimalRatio(mulByBigInt(subDecimals(current.close, previous.close), 100n), previous.close),
        volumeLongRatio: decimalRatio(mulByBigInt(this.sumWindow, longFactor), mulByBigInt(this.sumLong, windowFactor)),
        volumeShortRatio: decimalRatio(mulByBigInt(this.sumWindow, shortFactor), mulByBigInt(this.sumShort, windowFactor)),
        overLong, overShort, missingMinutes: this.missingInWindow,
      },
    };
  }

  private resetSeries(): void {
    this.volumes = [];
    this.missingInWindow = 0;
    this.sumWindow = ZERO_DECIMAL;
    this.sumShort = ZERO_DECIMAL;
    this.sumLong = ZERO_DECIMAL;
    this.closes = [];
  }
}
