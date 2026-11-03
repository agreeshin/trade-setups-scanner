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
    expect(gate.currentIntervalMs).toBe(
      DEFAULT_GATE_LIMITS.initialIntervalMs - 5 * DEFAULT_GATE_LIMITS.successStepMs,
    );
  });

  it("grows the interval after throttling", () => {
    const clock = new FakeClock();
    const gate = new AdaptiveGate(DEFAULT_GATE_LIMITS, clock);
    gate.reportThrottle();
    expect(gate.currentIntervalMs).toBe(DEFAULT_GATE_LIMITS.initialIntervalMs * 2);
  });

  it("honours explicit wait on throttle", async () => {
    const clock = new FakeClock(0);
    const gate = new AdaptiveGate(
      { ...DEFAULT_GATE_LIMITS, initialIntervalMs: 10, minIntervalMs: 5 },
      clock,
    );
    await gate.acquire();
    gate.reportThrottle(5000);
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
});
