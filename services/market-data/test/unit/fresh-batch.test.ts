import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import type { Kysely } from "kysely";
import type { MarketDataDB } from "../../src/db/schema.js";
import { FreshCollector, DEFAULT_FRESH_OPTIONS, type FreshOptions } from "../../src/ingest/fresh.js";
import { RealClock } from "../../src/ingest/clock.js";
import type { BybitClient } from "../../src/ingest/bybit.js";
import { insertCandleBatch } from "../../src/storage/candles.js";

vi.mock("../../src/storage/candles.js", async (original) => ({
  ...await original<typeof import("../../src/storage/candles.js")>(),
  insertCandleBatch: vi.fn(), insertCandles: vi.fn().mockResolvedValue({ inserted: 0, conflicting: 0 }),
}));
vi.mock("../../src/storage/watermarks.js", () => ({ initializeCollectionTails: vi.fn().mockResolvedValue(undefined) }));

const writer = vi.mocked(insertCandleBatch);
const bar = (n = 0, close = "1") => ({ openTime: new Date(Date.UTC(2026, 0, 1) + n * 60_000),
  open: "1", high: "2", low: "1", close, volume: "10", quoteTurnover: "15" });
let collector: FreshCollector;
async function make(count = 1, overrides: Partial<FreshOptions> = {}) {
  const rows = Array.from({ length: count }, (_, n) => ({ id: n + 1, symbol: `TEST${n}USDT`, tail_initialized: true }));
  const db = { selectFrom: (table: string) => ({
    innerJoin() { return this; }, where() { return this; }, select() { return this; }, limit() { return this; },
    execute: () => Promise.resolve(table.startsWith("collection_targets") ? rows : []),
  }) } as unknown as Kysely<MarketDataDB>;
  const client = { fetchClosedBars: () => Promise.resolve([]) } as unknown as BybitClient;
  collector = new FreshCollector(db, client, new RealClock(), { ...DEFAULT_FRESH_OPTIONS, ...overrides },
    () => undefined, { setSymbols: () => undefined, stop: () => undefined });
  await collector.pollOnce(); await collector.idle();
  return collector;
}
beforeEach(() => {
  vi.useFakeTimers(); vi.setSystemTime(new Date("2026-01-01T00:01:00Z"));
  writer.mockReset().mockImplementation((_db, rows) => Promise.resolve({ inserted: rows.length, conflicting: 0 }));
});
afterEach(async () => { await collector?.stop(); vi.useRealTimers(); });

describe("bounded confirmed WS microbatches", () => {
  it("amortizes a full 819-instrument burst into 13 bounded transactions rather than 819", async () => {
    const fresh = await make(819);
    for (let n = 0; n < 819; n++) expect(fresh.enqueue(`TEST${n}USDT`, bar())).toBe(true);
    await fresh.idle();
    expect(writer).toHaveBeenCalledTimes(13);
    expect(writer.mock.calls.flatMap((call) => call[1])).toHaveLength(819);
    expect(writer.mock.calls.every((call) => call[1].length <= 64)).toBe(true);
    expect(fresh.diagnostics()).toMatchObject({ committed: 819, pending: 0, bufferedBytes: 0, overflow: 0 });
    // A second burst must not be retained/deduplicated by a completed-key cache.
    for (let n = 0; n < 819; n++) fresh.enqueue(`TEST${n}USDT`, bar());
    await fresh.idle();
    expect(writer).toHaveBeenCalledTimes(26);
    expect(fresh.diagnostics()).toMatchObject({ pending: 0, bufferedBytes: 0, duplicates: 0 });
  });
  it("flushes a partial batch at dwell without waiting for another instrument", async () => {
    const fresh = await make(); fresh.enqueue("TEST0USDT", bar());
    await vi.advanceTimersByTimeAsync(24); expect(writer).not.toHaveBeenCalled();
    await vi.advanceTimersByTimeAsync(1); expect(writer).toHaveBeenCalledTimes(1);
    expect(fresh.diagnostics()).toMatchObject({ maxReceivedToAckMs: 25, maxQueueWaitMs: 25,
      lastReceivedAtMs: Date.parse("2026-01-01T00:01:00Z"), lastEnqueuedAtMs: Date.parse("2026-01-01T00:01:00Z"),
      lastAckAtMs: Date.parse("2026-01-01T00:01:00.025Z") });
  });
  it("accounts for in-flight rows/bytes and deduplicates only identical normalized observations", async () => {
    let release!: () => void;
    const hold = new Promise<void>((resolve) => { release = resolve; });
    writer.mockImplementation(async (_db, rows) => { await hold; return { inserted: rows.length, conflicting: 0 }; });
    const fresh = await make(1, { maxBufferedItems: 2, batchMaxRows: 1 });
    expect(fresh.enqueue("TEST0USDT", bar())).toBe(true);
    expect(fresh.enqueue("TEST0USDT", bar(0, "1.000"))).toBe(true);
    expect(fresh.enqueue("TEST0USDT", bar(0, "2"))).toBe(true);
    expect(fresh.enqueue("TEST0USDT", bar(1))).toBe(false);
    expect(fresh.diagnostics()).toMatchObject({ pending: 2, inFlight: 1, duplicates: 1, overflow: 1 });
    expect(fresh.diagnostics().bufferedBytes).toBeGreaterThan(0);
    release(); await fresh.idle();
    expect(writer.mock.calls.flatMap((call) => call[1]).map((x) => x.candle.close)).toEqual(["1", "2"]);
    expect(fresh.diagnostics()).toMatchObject({ pending: 0, bufferedBytes: 0 });
  });
  it("flushes at the byte cap and bounds retained bytes including the index", async () => {
    const size = Buffer.byteLength(JSON.stringify(bar()));
    const fresh = await make(3, { batchMaxBytes: size * 2 - 1 });
    fresh.enqueue("TEST0USDT", bar()); fresh.enqueue("TEST1USDT", bar()); fresh.enqueue("TEST2USDT", bar());
    await fresh.idle();
    expect(writer.mock.calls.every((call) => call[1].length === 1)).toBe(true);
    expect(fresh.enqueue("TEST0USDT", { ...bar(), close: "1".repeat(size * 2) })).toBe(false);
    await fresh.stop();
    const bounded = await make(1, { maxBufferedBytes: 1 });
    expect(bounded.enqueue("TEST0USDT", bar())).toBe(false);
    expect(bounded.diagnostics().pending).toBe(0);
  });
  it("retries the exact batch after an uncertain ACK and clears failures without committed success", async () => {
    writer.mockRejectedValueOnce(new Error("lost commit ACK"));
    const fresh = await make(); fresh.enqueue("TEST0USDT", bar()); await fresh.idle();
    expect(writer).toHaveBeenCalledTimes(2);
    expect(writer.mock.calls[0]![1]).toEqual(writer.mock.calls[1]![1]);
    expect(fresh.diagnostics()).toMatchObject({ retries: 1, committed: 1, pending: 0 });
    writer.mockRejectedValue(new Error("database unavailable"));
    fresh.enqueue("TEST0USDT", bar(1)); await fresh.idle();
    expect(fresh.diagnostics()).toMatchObject({ failed: 1, committed: 1, pending: 0, bufferedBytes: 0 });
  });
  it("stop cancels dwell, releases queued work and joins in-flight work before cleanup", async () => {
    const fresh = await make(); fresh.enqueue("TEST0USDT", bar()); await fresh.stop();
    await vi.advanceTimersByTimeAsync(100);
    expect(writer).not.toHaveBeenCalled();
    expect(fresh.diagnostics()).toMatchObject({ discardedOnStop: 1, pending: 0, bufferedBytes: 0 });
    let release!: () => void;
    writer.mockImplementation(async () => { await new Promise<void>((resolve) => { release = resolve; }); return { inserted: 1, conflicting: 0 }; });
    const second = await make(1, { batchMaxRows: 1 }); second.enqueue("TEST0USDT", bar());
    const stopping = second.stop(); expect(second.diagnostics().inFlight).toBe(1);
    release(); await stopping;
    expect(second.diagnostics()).toMatchObject({ pending: 0, bufferedBytes: 0, committed: 1 });
  });
});
