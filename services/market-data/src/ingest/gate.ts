import type { Clock } from "./clock.js";

export interface GateLimits {
  windowMs: number;
  maxPerWindow: number;
  initialIntervalMs: number;
  minIntervalMs: number;
  maxIntervalMs: number;
  successStepMs: number;
  throttleFactor: number;
  ipBanMinMs: number;
}

// Conservative defaults: ceiling is well below Bybit's documented default of
// 600 requests per 5 seconds per IP, leaving headroom for other traffic.
export const DEFAULT_GATE_LIMITS: GateLimits = {
  windowMs: 5000,
  maxPerWindow: 300,
  initialIntervalMs: 1200,
  minIntervalMs: 250,
  maxIntervalMs: 10_000,
  successStepMs: 100,
  throttleFactor: 2,
  ipBanMinMs: 600_000,
};

export interface GatePersistence {
  loadCooldown(): Promise<number | null>;
  saveCooldown(untilMs: number): Promise<void>;
}

export class AdaptiveGate {
  private intervalMs: number;
  private window: number[] = [];
  private lastAcquireMs: number | null = null;
  private cooldownUntilMs: number | null = null;
  private readonly initialized: Promise<void>;

  constructor(
    private readonly limits: GateLimits,
    private readonly clock: Clock,
    private readonly persistence?: GatePersistence,
  ) {
    this.intervalMs = limits.initialIntervalMs;
    this.initialized = this.restoreCooldown();
  }

  private async restoreCooldown(): Promise<void> {
    if (!this.persistence) {
      return;
    }
    const saved = await this.persistence.loadCooldown();
    if (saved !== null && saved > this.clock.nowMs()) {
      this.cooldownUntilMs = saved;
    }
  }

  async acquire(): Promise<void> {
    await this.initialized;
    for (;;) {
      const now = this.clock.nowMs();

      if (this.cooldownUntilMs !== null) {
        if (now < this.cooldownUntilMs) {
          await this.clock.sleep(this.cooldownUntilMs - now);
          continue;
        }
        this.cooldownUntilMs = null;
      }

      if (this.lastAcquireMs !== null) {
        const pacingWait = this.lastAcquireMs + this.intervalMs - now;
        if (pacingWait > 0) {
          await this.clock.sleep(pacingWait);
          continue;
        }
      }

      const cutoff = now - this.limits.windowMs;
      this.window = this.window.filter((timestamp) => timestamp > cutoff);
      if (this.window.length >= this.limits.maxPerWindow) {
        const oldest = this.window[0]!;
        await this.clock.sleep(oldest + this.limits.windowMs - now);
        continue;
      }

      const at = Math.max(now, (this.lastAcquireMs ?? now) + this.intervalMs);
      this.window.push(at);
      this.lastAcquireMs = at;
      return;
    }
  }

  reportSuccess(): void {
    this.intervalMs = Math.max(
      this.limits.minIntervalMs,
      this.intervalMs - this.limits.successStepMs,
    );
  }

  reportThrottle(explicitWaitMs?: number): void {
    this.intervalMs = Math.min(
      this.limits.maxIntervalMs,
      Math.round(this.intervalMs * this.limits.throttleFactor),
    );
    if (explicitWaitMs !== undefined && explicitWaitMs > 0) {
      const until = this.clock.nowMs() + explicitWaitMs;
      this.cooldownUntilMs = Math.max(this.cooldownUntilMs ?? 0, until);
    }
  }

  async reportIpBan(serverWaitMs?: number): Promise<void> {
    const now = this.clock.nowMs();
    const until = Math.max(
      now + this.limits.ipBanMinMs,
      serverWaitMs !== undefined ? now + serverWaitMs : 0,
    );
    this.cooldownUntilMs = until;
    if (this.persistence) {
      await this.persistence.saveCooldown(until);
    }
  }

  get currentIntervalMs(): number {
    return this.intervalMs;
  }
}
