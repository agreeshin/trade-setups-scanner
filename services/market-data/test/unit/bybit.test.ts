import { describe, expect, it, vi } from "vitest";
import { BybitClient, BybitApiError } from "../../src/ingest/bybit.js";
import { AdaptiveGate, DEFAULT_GATE_LIMITS } from "../../src/ingest/gate.js";
import { FakeClock } from "../../src/ingest/clock.js";
import type { TransportResponse } from "../../src/ingest/transport.js";

const MINUTE = 60_000;
const BASE = 1_700_000_000_000;

function okPage(times: number[]): TransportResponse {
  const list = times.map((ms) => [
    String(ms),
    "100",
    "101",
    "99",
    "100.5",
    "1.5",
    "150.75",
  ]);
  return {
    status: 200,
    headers: new Map(),
    bodyText: JSON.stringify({ retCode: 0, retMsg: "OK", result: { list } }),
  };
}

function errorResponse(status: number, body: object, headers?: Record<string, string>) {
  return {
    status,
    headers: new Map(Object.entries(headers ?? {})),
    bodyText: JSON.stringify(body),
  } satisfies TransportResponse;
}

function makeGate(clock: FakeClock) {
  const persistence = {
    loadCooldown: vi.fn(() => Promise.resolve(null)),
    saveCooldown: vi.fn(() => Promise.resolve()),
  };
  const gate = new AdaptiveGate(
    { ...DEFAULT_GATE_LIMITS, initialIntervalMs: 10, minIntervalMs: 5, ipBanMinMs: 600_000 },
    clock,
    persistence,
  );
  return { gate, persistence };
}

describe("BybitClient", () => {
  it("returns ascending bars with decimal strings from a single page", async () => {
    const clock = new FakeClock(BASE + 10 * MINUTE);
    const { gate } = makeGate(clock);
    const fetchMock = vi.fn(() => Promise.resolve(okPage([3 * MINUTE, MINUTE, 0])));
    const client = new BybitClient({ fetch: fetchMock }, gate, clock);
    const bars = await client.fetchClosedBars("TESTUSDT", 0, 3 * MINUTE);
    expect(bars.map((bar) => bar.openTime.getTime())).toEqual([
      0,
      MINUTE,
      3 * MINUTE,
    ]);
    expect(bars[0]?.close).toBe("100.5");
    expect(bars[0]?.volume).toBe("1.5");
    expect(bars[0]?.quoteTurnover).toBe("150.75");
  });

  it("paginates long ranges with multiple bounded windows", async () => {
    const clock = new FakeClock(BASE + 3000 * MINUTE);
    const { gate } = makeGate(clock);
    const requestedWindows: { start: string; end: string }[] = [];
    const fetchMock = vi.fn((url: string) => {
      const params = new URL(url).searchParams;
      requestedWindows.push({
        start: params.get("start") ?? "",
        end: params.get("end") ?? "",
      });
      const start = Number(params.get("start"));
      const end = Number(params.get("end"));
      const times: number[] = [];
      for (let ms = start; ms <= end; ms += MINUTE) {
        times.push(ms);
      }
      return Promise.resolve(okPage(times.reverse()));
    });
    const client = new BybitClient({ fetch: fetchMock }, gate, clock);
    const bars = await client.fetchClosedBars("TESTUSDT", 0, 2500 * MINUTE);
    expect(bars).toHaveLength(2501);
    expect(requestedWindows).toHaveLength(3);
    expect(Number(requestedWindows[0]?.start)).toBe(0);
    expect(Number(requestedWindows[1]?.start)).toBe(999 * MINUTE + MINUTE);
    expect(bars.at(-1)?.openTime.getTime()).toBe(2500 * MINUTE);
  });

  it("drops the still-open current minute", async () => {
    const nowMinute = 5 * MINUTE;
    const clock = new FakeClock(nowMinute + 30_000);
    const { gate } = makeGate(clock);
    const fetchMock = vi.fn(() => Promise.resolve(okPage([5 * MINUTE, 4 * MINUTE])));
    const client = new BybitClient({ fetch: fetchMock }, gate, clock);
    const bars = await client.fetchClosedBars("TESTUSDT", 0, 5 * MINUTE);
    expect(bars.map((bar) => bar.openTime.getTime())).toEqual([4 * MINUTE]);
  });

  it("retries after retCode 10006 rate limit", async () => {
    const clock = new FakeClock(BASE);
    const { gate } = makeGate(clock);
    const fetchMock = vi
      .fn<() => Promise<TransportResponse>>()
      .mockResolvedValueOnce(errorResponse(200, { retCode: 10006, retMsg: "Too many visits" }))
      .mockResolvedValueOnce(okPage([MINUTE]));
    const client = new BybitClient({ fetch: fetchMock }, gate, clock);
    const bars = await client.fetchClosedBars("TESTUSDT", MINUTE, MINUTE);
    expect(bars).toHaveLength(1);
    expect(fetchMock).toHaveBeenCalledTimes(2);
  });

  it("retries HTTP 429 honouring Retry-After", async () => {
    const clock = new FakeClock(BASE);
    const { gate } = makeGate(clock);
    const fetchMock = vi
      .fn<() => Promise<TransportResponse>>()
      .mockResolvedValueOnce(
        errorResponse(429, { retCode: 0, retMsg: "system frequency" }, { "retry-after": "2" }),
      )
      .mockResolvedValueOnce(okPage([MINUTE]));
    const client = new BybitClient({ fetch: fetchMock }, gate, clock);
    const bars = await client.fetchClosedBars("TESTUSDT", MINUTE, MINUTE);
    expect(bars).toHaveLength(1);
    expect(clock.now).toBeGreaterThanOrEqual(2000);
  });

  it("persists a >=600s cooldown on 403 access too frequent and retries", async () => {
    const clock = new FakeClock(BASE);
    const { gate, persistence } = makeGate(clock);
    const fetchMock = vi
      .fn<() => Promise<TransportResponse>>()
      .mockResolvedValueOnce(
        errorResponse(403, { retCode: 10010, retMsg: "access too frequent" }),
      )
      .mockResolvedValueOnce(okPage([MINUTE]));
    const client = new BybitClient({ fetch: fetchMock }, gate, clock);
    const bars = await client.fetchClosedBars("TESTUSDT", MINUTE, MINUTE);
    expect(bars).toHaveLength(1);
    expect(persistence.saveCooldown).toHaveBeenCalledTimes(1);
    expect(clock.now).toBeGreaterThanOrEqual(600_000);
  });

  it("fails fast on a non-throttling 403", async () => {
    const clock = new FakeClock(BASE);
    const { gate } = makeGate(clock);
    const fetchMock = vi.fn(() =>
      Promise.resolve(errorResponse(403, { retCode: 10003, retMsg: "api key invalid" })),
    );
    const client = new BybitClient({ fetch: fetchMock }, gate, clock);
    await expect(client.fetchClosedBars("TESTUSDT", MINUTE, MINUTE)).rejects.toBeInstanceOf(
      BybitApiError,
    );
    expect(fetchMock).toHaveBeenCalledTimes(1);
  });

  it("fails with the last error after exhausting retries", async () => {
    const clock = new FakeClock(BASE);
    const { gate } = makeGate(clock);
    const fetchMock = vi.fn(() =>
      Promise.resolve(errorResponse(200, { retCode: 10006, retMsg: "Too many visits" })),
    );
    const client = new BybitClient({ fetch: fetchMock }, gate, clock);
    await expect(client.fetchClosedBars("TESTUSDT", MINUTE, MINUTE)).rejects.toThrow(
      "retCode 10006",
    );
  });

  it("honours HTTP-date Retry-After and 10006 reset across fresh/history requests", async () => {
    const clock = new FakeClock(BASE);
    const { gate } = makeGate(clock);
    const fetchMock = vi.fn<() => Promise<TransportResponse>>()
      .mockResolvedValueOnce(errorResponse(429, {}, { "retry-after": new Date(BASE + 10_000).toUTCString() }))
      .mockResolvedValueOnce(errorResponse(200, { retCode: 10006 }, { "x-bapi-limit-reset-timestamp": String(BASE + 20_000) }))
      .mockResolvedValueOnce(okPage([MINUTE]));
    const client = new BybitClient({ fetch: fetchMock }, gate, clock);
    await expect(client.fetchClosedBars("TESTUSDT", MINUTE, MINUTE, { priority: "fresh", maxAttempts: 1 })).rejects.toThrow("429");
    await expect(client.fetchLinearPerpetualUsdtSymbols()).resolves.toEqual([]);
    expect(clock.nowMs()).toBeGreaterThanOrEqual(BASE + 20_000);
    client.close();
  });

  it("rejects malformed decimals/result lists instead of manufacturing absence", async () => {
    const clock = new FakeClock(BASE);
    const { gate } = makeGate(clock);
    const fetchMock = vi.fn<() => Promise<TransportResponse>>()
      .mockResolvedValueOnce(errorResponse(200, { retCode: 0, result: {} }))
      .mockResolvedValueOnce(errorResponse(200, { retCode: 0, result: { list: [[String(MINUTE), 1, "1", "1", "1", "1", "1"]] } }));
    const client = new BybitClient({ fetch: fetchMock }, gate, clock);
    await expect(client.fetchClosedBars("TESTUSDT", MINUTE, MINUTE)).rejects.toThrow("result list");
    await expect(client.fetchClosedBars("TESTUSDT", MINUTE, MINUTE)).rejects.toThrow("decimal");
    client.close();
  });
});
