import { describe, expect, it, vi } from "vitest";
import { AdaptiveGate, DEFAULT_GATE_LIMITS, type GatePersistence } from "../../src/ingest/gate.js";
import { FakeClock } from "../../src/ingest/clock.js";

describe("AdaptiveGate", () => {
  it("enforces pacing interval between acquisitions", async () => {
    const clock = new FakeClock(0);
    const gate = new AdaptiveGate(DEFAULT_GATE_LIMITS, clock);
    await gate.acquire();
    await gate.acquire();
    expect(clock.now).toBeGreaterThanOrEqual(DEFAULT_GATE_LIMITS.initialIntervalMs);
  });

  it("limits the sliding request window", async () => {
    const clock = new FakeClock(0);
    const gate = new AdaptiveGate(
      { ...DEFAULT_GATE_LIMITS, maxPerWindow: 2, initialIntervalMs: 10, minIntervalMs: 5 },
      clock,
    );
    await gate.acquire();
    await gate.acquire();
    await gate.acquire();
    expect(clock.now).toBeGreaterThanOrEqual(DEFAULT_GATE_LIMITS.windowMs);
  });

  it("shrinks the interval after successes", () => {
    const clock = new FakeClock();
    const gate = new AdaptiveGate(DEFAULT_GATE_LIMITS, clock);
    for (let i = 0; i < 5; i += 1) {
      gate.reportSuccess();
    }
    expect(gate.currentIntervalMs).toBe(83);
  });

  it("grows the interval after throttling", async () => {
    const clock = new FakeClock();
    const gate = new AdaptiveGate(DEFAULT_GATE_LIMITS, clock);
    await gate.reportThrottle();
    expect(gate.currentIntervalMs).toBe(DEFAULT_GATE_LIMITS.initialIntervalMs * 2);
  });

  it("honours explicit wait on throttle", async () => {
    const clock = new FakeClock(0);
    const gate = new AdaptiveGate(
      { ...DEFAULT_GATE_LIMITS, initialIntervalMs: 10, minIntervalMs: 5 },
      clock,
    );
    await gate.acquire();
    await gate.reportThrottle(5000);
    await gate.acquire();
    expect(clock.now).toBeGreaterThanOrEqual(5000);
  });

  it("applies and persists a >=600s cooldown on ip ban", async () => {
    const clock = new FakeClock(0);
    const saveCooldown = vi.fn(() => Promise.resolve());
    const persistence: GatePersistence = {
      loadCooldown: vi.fn(() => Promise.resolve(null)),
      saveCooldown,
    };
    const gate = new AdaptiveGate(DEFAULT_GATE_LIMITS, clock, persistence);
    await gate.acquire();
    await gate.reportIpBan();
    expect(saveCooldown).toHaveBeenCalledWith(
      DEFAULT_GATE_LIMITS.ipBanMinMs,
    );
    await gate.acquire();
    expect(clock.now).toBeGreaterThanOrEqual(DEFAULT_GATE_LIMITS.ipBanMinMs);
  });

  it("restores a persisted cooldown on startup", async () => {
    const clock = new FakeClock(0);
    const persistence: GatePersistence = {
      loadCooldown: vi.fn(() => Promise.resolve(5000)),
      saveCooldown: vi.fn(() => Promise.resolve()),
    };
    const gate = new AdaptiveGate(
      { ...DEFAULT_GATE_LIMITS, initialIntervalMs: 10, minIntervalMs: 5 },
      clock,
      persistence,
    );
    await gate.acquire();
    expect(clock.now).toBeGreaterThanOrEqual(5000);
  });

  it("completes 820 fresh requests with >=25-ish rps and history progress under backlog", async () => {
    const clock = new FakeClock(0);
    const gate = new AdaptiveGate({ ...DEFAULT_GATE_LIMITS, initialIntervalMs: 34, successStepMs: 0 }, clock);
    const order: string[] = [];
    let fresh = 0;
    let completed = 0;
    let lastFreshAt = 0;
    const producer = async () => {
      for (;;) {
        const n = fresh++;
        if (n >= 820) return;
        await gate.run(() => {
          order.push("fresh"); completed += 1; lastFreshAt = clock.nowMs();
          return Promise.resolve();
        }, "fresh");
      }
    };
    const history = async () => {
      for (let n = 0; n < 140; n++) await gate.run(() => { order.push("history"); return Promise.resolve(); }, "history");
    };
    await Promise.all([history(), ...Array.from({ length: 8 }, producer)]);
    expect(completed).toBe(820);
    expect(order.filter((kind) => kind === "history")).toHaveLength(140);
    expect(lastFreshAt).toBeLessThanOrEqual(33_000);
    expect(820 / (lastFreshAt / 1000)).toBeGreaterThan(25);
    let burst = 0;
    for (const kind of order.slice(0, 900)) {
      burst = kind === "fresh" ? burst + 1 : 0;
      expect(burst).toBeLessThanOrEqual(6);
    }
    gate.stop();
  });

  it("bounds concurrency/queue and releases cancelled waiters", async () => {
    const clock = new FakeClock();
    const gate = new AdaptiveGate({ ...DEFAULT_GATE_LIMITS, maxConcurrent: 1, maxQueued: 1 }, clock);
    let release = () => undefined as void;
    const block = { promise: new Promise<void>((resolve) => { release = resolve; }), resolve: () => release() };
    const running = gate.run(() => block.promise);
    // Let the first permit enter the operation.
    await Promise.resolve(); await Promise.resolve(); await Promise.resolve();
    const controller = new AbortController();
    const queued = gate.acquire("fresh", controller.signal).catch((error: unknown) => error);
    await expect(gate.acquire()).rejects.toThrow("queue full");
    controller.abort();
    expect(await queued).toBeInstanceOf(Error);
    block.resolve(); await running;
    await gate.acquire();
    gate.stop();
    await expect(gate.acquire()).rejects.toThrow("stopped");
  });

  it("persists throttling and never shortens a prior ban", async () => {
    const clock = new FakeClock();
    const saveCooldown = vi.fn(() => Promise.resolve());
    const gate = new AdaptiveGate(DEFAULT_GATE_LIMITS, clock, { loadCooldown: () => Promise.resolve(null), saveCooldown });
    await gate.reportIpBan(900_000);
    await gate.reportIpBan(600_000);
    await gate.reportThrottle(1000);
    expect(saveCooldown.mock.calls).toHaveLength(3);
    expect(saveCooldown).toHaveBeenLastCalledWith(900_000);
    await gate.acquire("fresh");
    expect(clock.nowMs()).toBeGreaterThanOrEqual(900_000);
  });
  it("recovers cautiously after a ban without hours of 1ms linear acceleration", async () => {
    const clock = new FakeClock();
    const gate = new AdaptiveGate(DEFAULT_GATE_LIMITS, clock);
    await gate.reportIpBan();
    for (let n = 0; n < 200; n++) { await gate.acquire(); gate.reportSuccess(); }
    expect(gate.currentIntervalMs).toBe(DEFAULT_GATE_LIMITS.minIntervalMs);
    expect(clock.nowMs()).toBeGreaterThanOrEqual(600_000);
    expect(clock.nowMs()).toBeLessThan(900_000);
  });
});
