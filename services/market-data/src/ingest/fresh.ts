import type { Kysely } from "kysely";
import type { MarketDataDB } from "../db/schema.js";
import { candleObservationKey, insertCandleBatch, insertCandles } from "../storage/candles.js";
import { initializeCollectionTails } from "../storage/watermarks.js";
import type { BybitClient, KlineBar } from "./bybit.js";
import type { Clock } from "./clock.js";
import { BybitClosedStream, DEFAULT_WS_OPTIONS, type WsOptions } from "./websocket.js";

const MINUTE = 60_000;
export interface FreshOptions {
  tickMs: number;
  fallbackDelayMs: number;
  repairMinutes: number;
  maxSymbols: number;
  concurrency: number;
  maxBufferedItems: number;
  maxBufferedBytes: number;
  batchMaxRows: number;
  batchMaxBytes: number;
  batchDwellMs: number;
  batchTransactionTimeoutMs: number;
  ws: WsOptions;
}
export const DEFAULT_FRESH_OPTIONS: FreshOptions = {
  tickMs: 2000, fallbackDelayMs: 3000, repairMinutes: 5,
  maxSymbols: 4096, concurrency: 8, maxBufferedItems: 4096,
  maxBufferedBytes: 4 * 1024 * 1024, ws: DEFAULT_WS_OPTIONS,
  batchMaxRows: 64, batchMaxBytes: 64 * 1024, batchDwellMs: 25, batchTransactionTimeoutMs: 5000,
};
interface Target { id: number; symbol: string; retryAt: number }
interface Repair { target: Target; from: number; to: number }
interface Write { id: number; symbol: string; bar: KlineBar; bytes: number; payloadBytes: number; key: string; receivedAt: number; enqueuedAt: number }

/** Bounded fresh I/O owner, independent of the single serial historical drain. */
export class FreshCollector {
  private readonly lifetime = new AbortController();
  private readonly targets = new Map<string, Target>();
  private readonly repairs = new Map<number, Repair>();
  private readonly repairing = new Set<number>();
  private readonly writes: Write[] = [];
  private bufferedBytes = 0;
  // Owns queued AND in-flight observations until commit/failure. Index keys and
  // cloned payloads are charged together; no completed-candle cache is retained.
  private readonly pending = new Map<string, Write>();
  private dwell: ReturnType<typeof setTimeout> | undefined;
  private lastSummaryAt = -Infinity;
  private readonly counters = { received: 0, duplicates: 0, overflow: 0, committed: 0, inserted: 0,
    conflicts: 0, failed: 0, discardedOnStop: 0, batches: 0, retries: 0, maxReceivedToAckMs: 0, maxWriteMs: 0,
    maxQueueWaitMs: 0, lastReceivedAtMs: null as number | null, lastEnqueuedAtMs: null as number | null,
    lastAckAtMs: null as number | null };
  private readonly workers = new Set<Promise<void>>();
  private readonly writeWorkers = new Set<Promise<void>>();
  private readonly errors = new Map<string, { at: number; count: number }>();
  private readonly stream: Pick<BybitClosedStream, "setSymbols" | "stop">;
  private polling: Promise<void> | undefined;
  private timer: ReturnType<typeof setInterval> | undefined;

  constructor(
    private readonly db: Kysely<MarketDataDB>,
    private readonly client: BybitClient,
    private readonly clock: Clock,
    private readonly options: FreshOptions = DEFAULT_FRESH_OPTIONS,
    private readonly onEvent: (event: string, detail: string) => void = (event, detail) => console.info(event, detail),
    stream?: Pick<BybitClosedStream, "setSymbols" | "stop">,
  ) {
    for (const value of [options.batchMaxRows, options.batchMaxBytes, options.batchDwellMs, options.batchTransactionTimeoutMs])
      if (!Number.isSafeInteger(value) || value < 1) throw new Error("invalid fresh batch limits");
    this.stream = stream ?? new BybitClosedStream(options.ws, (symbol, bar) => this.enqueue(symbol, bar), onEvent, () => clock.nowMs());
  }

  async start(): Promise<void> {
    await this.pollOnce();
    this.timer = setInterval(() => { void this.pollOnce().catch((error: unknown) => this.error("fresh_poll_failed", error)); }, this.options.tickMs);
  }

  pollOnce(): Promise<void> {
    if (this.polling) return this.polling;
    if (this.lifetime.signal.aborted) return Promise.resolve();
    this.polling = this.poll().finally(() => { this.polling = undefined; });
    return this.polling;
  }

  private async poll(): Promise<void> {
    const rows = await this.db.selectFrom("collection_targets as t")
      .innerJoin("instruments as i", "i.id", "t.instrument_id")
      .innerJoin("exchanges as e", "e.id", "i.exchange_id")
      .where("t.enabled", "=", true).where("i.status", "=", "active")
      .where("e.code", "=", "bybit").where("i.category", "=", "linear")
      .select(["i.id", "i.symbol", "t.tail_initialized"]).limit(this.options.maxSymbols + 1).execute();
    if (rows.length > this.options.maxSymbols) throw new Error("fresh symbol budget exceeded");
    await initializeCollectionTails(this.db, rows.filter((row) => !row.tail_initialized).map((row) => row.id));
    if (this.lifetime.signal.aborted) return;
    const symbols = new Set(rows.map((row) => row.symbol));
    for (const [symbol, target] of this.targets) if (!symbols.has(symbol)) {
      this.targets.delete(symbol); this.repairs.delete(target.id);
    }
    for (const row of rows) if (!this.targets.has(row.symbol)) this.targets.set(row.symbol, { ...row, retryAt: 0 });
    this.stream.setSymbols([...symbols]);
    if (rows.length === 0) return;
    const now = this.clock.nowMs();
    const to = Math.floor((now - this.options.fallbackDelayMs) / MINUTE) * MINUTE - MINUTE;
    const from = to - (this.options.repairMinutes - 1) * MINUTE;
    const present = await this.db.selectFrom("candles").select(["instrument_id", "open_time"])
      .where("instrument_id", "in", rows.map((row) => row.id))
      .where("open_time", ">=", new Date(from)).where("open_time", "<=", new Date(to))
      .limit(this.options.maxSymbols * this.options.repairMinutes).execute();
    const times = new Set(present.map((row) => `${row.instrument_id}:${row.open_time.getTime()}`));
    for (const target of this.targets.values()) {
      if (target.retryAt > now || this.repairing.has(target.id) || this.repairs.has(target.id)) continue;
      let first: number | undefined;
      for (let minute = from; minute <= to; minute += MINUTE) {
        if (!times.has(`${target.id}:${minute}`)) { first = minute; break; }
      }
      if (first !== undefined) this.repairs.set(target.id, { target, from: first, to });
    }
    this.launchWorkers();
  }

  enqueue(symbol: string, bar: KlineBar): boolean {
    const receivedAt = this.clock.nowMs(); // Actual confirmed-WS callback, NOT candle close or SQL now().
    const target = this.targets.get(symbol);
    if (!target || this.lifetime.signal.aborted) return true;
    this.counters.received++;
    this.counters.lastReceivedAtMs = receivedAt;
    const payloadBytes = Buffer.byteLength(JSON.stringify(bar));
    if (payloadBytes > this.options.batchMaxBytes) { this.counters.overflow++; this.summary(); return false; }
    const key = candleObservationKey(target.id, bar);
    if (this.pending.has(key)) { this.counters.duplicates++; return true; }
    // Conservative allowance for UTF-16 payload/key copies, object/index slots
    // and bounded serialization scratch, including an in-flight retry batch.
    const bytes = payloadBytes * 4 + Buffer.byteLength(key) * 2 + 512;
    if (this.pending.size >= this.options.maxBufferedItems || this.bufferedBytes + bytes > this.options.maxBufferedBytes) {
      this.counters.overflow++; this.summary(); return false;
    }
    const write = { id: target.id, symbol, bar: { ...bar, openTime: new Date(bar.openTime) }, bytes, payloadBytes, key, receivedAt,
      enqueuedAt: this.clock.nowMs() };
    this.counters.lastEnqueuedAtMs = write.enqueuedAt;
    this.writes.push(write); this.pending.set(key, write);
    this.bufferedBytes += bytes;
    this.launchWorkers();
    return true;
  }

  private launchWorkers(force = false): void {
    // One bounded multi-instrument transaction amortizes WAL commits. REST
    // admission/ban waiters retain their independent existing worker capacity.
    if (!this.lifetime.signal.aborted && this.writeWorkers.size === 0 && this.writes.length) {
      const due = this.clock.nowMs() - this.writes[0]!.receivedAt >= this.options.batchDwellMs;
      const queuedBytes = this.writes.reduce((sum, write) => sum + write.payloadBytes, 0);
      if (!force && !due && this.writes.length < this.options.batchMaxRows && queuedBytes < this.options.batchMaxBytes) {
        this.dwell ??= setTimeout(() => { this.dwell = undefined; this.launchWorkers(true); }, this.options.batchDwellMs);
      } else {
        clearTimeout(this.dwell); this.dwell = undefined;
        const batch: Write[] = [];
        let bytes = 0;
        while (this.writes.length && batch.length < this.options.batchMaxRows) {
          const next = this.writes[0]!;
          if (bytes + next.payloadBytes > this.options.batchMaxBytes) break;
          batch.push(this.writes.shift()!); bytes += next.payloadBytes;
        }
        const writer = this.writeBatch(batch).finally(() => { this.writeWorkers.delete(writer); this.launchWorkers(); });
        this.writeWorkers.add(writer);
      }
    }
    while (!this.lifetime.signal.aborted && this.workers.size < this.options.concurrency && this.repairs.size) {
      // Start in a microtask so registration precedes synchronous completion.
      const worker = Promise.resolve().then(() => this.work()).catch((error: unknown) => this.error("fresh_worker_failed", error))
        .finally(() => { this.workers.delete(worker); this.launchWorkers(); });
      this.workers.add(worker);
    }
  }

  private release(write: Write): void {
    if (this.pending.delete(write.key)) this.bufferedBytes -= write.bytes;
  }

  private async writeBatch(batch: Write[]): Promise<void> {
    const started = this.clock.nowMs();
    try {
      const observations = batch.filter((write) => this.targets.get(write.symbol)?.id === write.id);
      for (const write of observations) this.counters.maxQueueWaitMs = Math.max(this.counters.maxQueueWaitMs, started - write.enqueuedAt);
      for (let attempt = 0; attempt < 2; attempt++) {
        if (this.lifetime.signal.aborted) { this.counters.discardedOnStop += batch.length; return; }
        try {
          const result = await insertCandleBatch(this.db, observations.map((write) => ({ instrumentId: write.id, candle: write.bar })),
            this.options.batchTransactionTimeoutMs);
          const ack = this.clock.nowMs();
          this.counters.lastAckAtMs = ack;
          this.counters.batches++; this.counters.committed += observations.length;
          this.counters.inserted += result.inserted; this.counters.conflicts += result.conflicting;
          this.counters.maxWriteMs = Math.max(this.counters.maxWriteMs, ack - started);
          for (const write of observations) this.counters.maxReceivedToAckMs = Math.max(this.counters.maxReceivedToAckMs, ack - write.receivedAt);
          return;
        } catch (error) {
          // Re-submit the exact bounded observations after an uncertain ACK.
          // Add-only insertion/journal RETURNING makes committed retries safe.
          this.error("fresh_write_failed", error);
          if (attempt === 0) this.counters.retries++;
          else this.counters.failed += observations.length;
        }
      }
      // Failure remains visible; poll/REST and durable tail repair retry holes.
    } finally { for (const write of batch) this.release(write); this.summary(); }
  }

  diagnostics() {
    return { ...this.counters, pending: this.pending.size, queued: this.writes.length,
      inFlight: this.pending.size - this.writes.length, bufferedBytes: this.bufferedBytes,
      oldestAgeMs: this.pending.size ? Math.max(0, this.clock.nowMs() - this.pending.values().next().value!.receivedAt) : 0 };
  }

  private summary(force = false): void {
    const now = this.clock.nowMs();
    if (!force && now - this.lastSummaryAt < 30_000) return;
    this.lastSummaryAt = now;
    this.onEvent("fresh_write_summary", JSON.stringify(this.diagnostics()));
  }

  private async work(): Promise<void> {
    while (!this.lifetime.signal.aborted) {
      const repair = this.repairs.values().next().value;
      if (!repair) return;
      this.repairs.delete(repair.target.id);
      this.repairing.add(repair.target.id);
      try {
        // One bounded attempt: future polls retry absence/failure without a
        // six-attempt per-symbol chain holding up the entire fresh sweep.
        const bars = await this.client.fetchClosedBars(repair.target.symbol, repair.from, repair.to,
          { priority: "fresh", maxAttempts: 1, signal: this.lifetime.signal });
        if (!this.lifetime.signal.aborted && this.targets.get(repair.target.symbol)?.id === repair.target.id)
          await insertCandles(this.db, repair.target.id, bars);
        repair.target.retryAt = this.clock.nowMs() + 10_000;
        // No gap_verifications writes here: empty fresh data remains pending.
      } catch (error) {
        repair.target.retryAt = this.clock.nowMs() + 15_000;
        if (!this.lifetime.signal.aborted) this.error("fresh_rest_failed", error);
      } finally { this.repairing.delete(repair.target.id); }
    }
  }

  private error(event: string, error: unknown): void {
    const old = this.errors.get(event);
    const now = this.clock.nowMs();
    if (old && now - old.at < 30_000) { old.count += 1; return; }
    this.onEvent(event, `${(error instanceof Error ? error.message : String(error)).slice(0, 180)}; repeats=${old?.count ?? 0}`);
    this.errors.set(event, { at: now, count: 0 });
  }

  async idle(): Promise<void> {
    while (this.workers.size || this.writeWorkers.size || this.writes.length) {
      this.launchWorkers(true);
      await Promise.all([...this.workers, ...this.writeWorkers]);
    }
  }

  async stop(): Promise<void> {
    clearInterval(this.timer);
    clearTimeout(this.dwell); this.dwell = undefined;
    this.lifetime.abort(new Error("fresh collector stopped"));
    this.stream.stop(); this.repairs.clear();
    this.counters.discardedOnStop += this.writes.length;
    for (const write of this.writes) this.release(write);
    this.writes.length = 0;
    await this.polling;
    await this.idle();
    this.targets.clear(); this.repairing.clear(); this.errors.clear();
    this.summary(true);
  }
}
