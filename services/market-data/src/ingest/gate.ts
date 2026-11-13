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
  maxConcurrent?: number;
  maxQueued?: number;
  freshBurst?: number;
}

// One aggregate budget, not one budget per consumer/lane. Below 600/5s/IP.
export const DEFAULT_GATE_LIMITS: GateLimits = {
  windowMs: 5000, maxPerWindow: 150,
  initialIntervalMs: 100, minIntervalMs: 34, maxIntervalMs: 10_000,
  successStepMs: 1, throttleFactor: 2, ipBanMinMs: 600_000,
  maxConcurrent: 8, maxQueued: 256, freshBurst: 6,
};

export type RequestPriority = "fresh" | "history";
export interface GatePersistence {
  loadCooldown(): Promise<number | null>;
  saveCooldown(untilMs: number): Promise<void>;
}
interface Waiter {
  priority: RequestPriority;
  resolve: (release: () => void) => void;
  reject: (reason: unknown) => void;
  signal: AbortSignal | undefined;
  abort: () => void;
}

/** Owns a bounded admission queue and in-flight permits; release is mandatory. */
export class AdaptiveGate {
  private intervalMs: number;
  private window: number[] = [];
  private lastAcquireMs: number | null = null;
  private cooldownUntilMs = 0;
  private readonly initialized: Promise<void>;
  private readonly queue: Waiter[] = [];
  private readonly lifetime = new AbortController();
  private pumping = false;
  private active = 0;
  private freshGrants = 0;

  constructor(
    private readonly limits: GateLimits,
    private readonly clock: Clock,
    private readonly persistence?: GatePersistence,
  ) {
    this.intervalMs = limits.initialIntervalMs;
    this.initialized = this.restoreCooldown();
  }

  private async restoreCooldown(): Promise<void> {
    const saved = await this.persistence?.loadCooldown();
    if (saved !== undefined && saved !== null) this.cooldownUntilMs = Math.max(this.cooldownUntilMs, saved);
  }

  // Compatibility for callers needing only a pacing token.
  async acquire(priority: RequestPriority = "history", signal?: AbortSignal): Promise<void> {
    const release = await this.permit(priority, signal);
    release();
  }

  async run<T>(operation: () => Promise<T>, priority: RequestPriority = "history", signal?: AbortSignal): Promise<T> {
    const release = await this.permit(priority, signal);
    try {
      signal?.throwIfAborted();
      return await operation();
    } finally { release(); }
  }

  private permit(priority: RequestPriority, signal?: AbortSignal): Promise<() => void> {
    if (this.lifetime.signal.aborted) return Promise.reject(new Error("Bybit gate stopped"));
    if (signal?.aborted) return Promise.reject(new Error("Bybit request aborted", { cause: signal.reason }));
    if (this.queue.length >= (this.limits.maxQueued ?? 256)) return Promise.reject(new Error("Bybit admission queue full"));
    return new Promise((resolve, reject) => {
      const waiter: Waiter = { priority, resolve, reject, signal, abort: () => {
        const index = this.queue.indexOf(waiter);
        if (index !== -1) this.queue.splice(index, 1);
        reject(new Error("Bybit request aborted", { cause: signal?.reason }));
      } };
      signal?.addEventListener("abort", waiter.abort, { once: true });
      this.queue.push(waiter);
      void this.pump();
    });
  }

  private async pump(): Promise<void> {
    if (this.pumping || this.lifetime.signal.aborted) return;
    this.pumping = true;
    try {
      await this.initialized;
      while (this.queue.length > 0 && this.active < (this.limits.maxConcurrent ?? 8)) {
        const now = this.clock.nowMs();
        this.window = this.window.filter((timestamp) => timestamp > now - this.limits.windowMs);
        const next = Math.max(this.cooldownUntilMs,
          this.lastAcquireMs === null ? now : this.lastAcquireMs + this.intervalMs,
          this.window.length >= this.limits.maxPerWindow ? this.window[0]! + this.limits.windowMs : now);
        if (next > now) {
          await this.clock.sleep(next - now, this.lifetime.signal);
          continue; // Recheck cooldown and priorities after waiting.
        }
        const fresh = this.queue.findIndex((item) => item.priority === "fresh");
        const history = this.queue.findIndex((item) => item.priority === "history");
        const index = fresh !== -1 && (history === -1 || this.freshGrants < (this.limits.freshBurst ?? 4)) ? fresh : history;
        const waiter = this.queue.splice(index, 1)[0]!;
        waiter.signal?.removeEventListener("abort", waiter.abort);
        if (waiter.signal?.aborted) { waiter.reject(waiter.signal.reason); continue; }
        this.freshGrants = waiter.priority === "fresh" ? this.freshGrants + 1 : 0;
        this.active += 1;
        this.window.push(now);
        this.lastAcquireMs = now;
        let released = false;
        waiter.resolve(() => {
          if (released) return;
          released = true;
          this.active -= 1;
          void this.pump();
        });
      }
    } catch (error) {
      this.rejectQueued(error);
    } finally {
      this.pumping = false;
      if (this.queue.length > 0 && this.active < (this.limits.maxConcurrent ?? 8)) void this.pump();
    }
  }

  private rejectQueued(error: unknown): void {
    for (const waiter of this.queue.splice(0)) {
      waiter.signal?.removeEventListener("abort", waiter.abort);
      waiter.reject(error);
    }
  }

  stop(): void {
    this.lifetime.abort();
    this.rejectQueued(new Error("Bybit gate stopped"));
  }

  reportSuccess(): void {
    // A 1ms linear step is appropriate near the 34ms ceiling, but would take
    // hours to recover from a 10s ban interval. Close 5% of the remaining gap.
    const step = Math.max(this.limits.successStepMs, Math.ceil((this.intervalMs - this.limits.minIntervalMs) / 20));
    this.intervalMs = Math.max(this.limits.minIntervalMs, this.intervalMs - step);
  }

  async reportThrottle(explicitWaitMs = 1000): Promise<void> {
    this.intervalMs = Math.min(this.limits.maxIntervalMs, Math.round(this.intervalMs * this.limits.throttleFactor));
    await this.pause(explicitWaitMs);
  }

  private async pause(waitMs: number): Promise<void> {
    this.cooldownUntilMs = Math.max(this.cooldownUntilMs, this.clock.nowMs() + waitMs);
    await this.persistence?.saveCooldown(this.cooldownUntilMs);
  }

  async reportIpBan(serverWaitMs = 0): Promise<void> {
    this.intervalMs = this.limits.maxIntervalMs;
    await this.pause(Math.max(this.limits.ipBanMinMs, serverWaitMs));
  }

  get currentIntervalMs(): number { return this.intervalMs; }
}
