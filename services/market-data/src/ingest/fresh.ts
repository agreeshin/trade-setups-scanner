import type { Kysely } from "kysely";
import type { MarketDataDB } from "../db/schema.js";
import { insertCandles } from "../storage/candles.js";
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
  ws: WsOptions;
}
export const DEFAULT_FRESH_OPTIONS: FreshOptions = {
  tickMs: 2000, fallbackDelayMs: 3000, repairMinutes: 5,
  maxSymbols: 4096, concurrency: 8, maxBufferedItems: 4096,
  maxBufferedBytes: 4 * 1024 * 1024, ws: DEFAULT_WS_OPTIONS,
};
interface Target { id: number; symbol: string; retryAt: number }
interface Repair { target: Target; from: number; to: number }
interface Write { id: number; symbol: string; bar: KlineBar; bytes: number }

/** Bounded fresh I/O owner, independent of the single serial historical drain. */
export class FreshCollector {
  private readonly lifetime = new AbortController();
  private readonly targets = new Map<string, Target>();
  private readonly repairs = new Map<number, Repair>();
  private readonly repairing = new Set<number>();
  private readonly writes: Write[] = [];
  private bufferedBytes = 0;
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
    const target = this.targets.get(symbol);
    if (!target || this.lifetime.signal.aborted) return true;
    const bytes = Buffer.byteLength(JSON.stringify(bar));
    if (this.writes.length >= this.options.maxBufferedItems || this.bufferedBytes + bytes > this.options.maxBufferedBytes) return false;
    this.writes.push({ id: target.id, symbol, bar, bytes });
    this.bufferedBytes += bytes;
    this.launchWorkers();
    return true;
  }

  private launchWorkers(): void {
    // WS writes have their own capacity: eight REST waiters during a ban must
    // never occupy the writer slots or stop already confirmed WS candles.
    while (!this.lifetime.signal.aborted && this.writeWorkers.size < 2 && this.writes.length) {
      const writer = Promise.resolve().then(async () => {
        while (!this.lifetime.signal.aborted) {
          const write = this.writes.shift();
          if (!write) return;
          this.bufferedBytes -= write.bytes;
          if (this.targets.get(write.symbol)?.id === write.id) await insertCandles(this.db, write.id, [write.bar]);
        }
      }).catch((error: unknown) => this.error("fresh_write_failed", error))
        .finally(() => { this.writeWorkers.delete(writer); this.launchWorkers(); });
      this.writeWorkers.add(writer);
    }
    while (!this.lifetime.signal.aborted && this.workers.size < this.options.concurrency && this.repairs.size) {
      // Start in a microtask so registration precedes synchronous completion.
      const worker = Promise.resolve().then(() => this.work()).catch((error: unknown) => this.error("fresh_worker_failed", error))
        .finally(() => { this.workers.delete(worker); this.launchWorkers(); });
      this.workers.add(worker);
    }
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

  async idle(): Promise<void> { while (this.workers.size || this.writeWorkers.size) await Promise.all([...this.workers, ...this.writeWorkers]); }

  async stop(): Promise<void> {
    clearInterval(this.timer);
    this.lifetime.abort(new Error("fresh collector stopped"));
    this.stream.stop(); this.repairs.clear(); this.writes.length = 0; this.bufferedBytes = 0;
    await this.polling;
    await this.idle();
    this.targets.clear(); this.repairing.clear(); this.errors.clear();
  }
}
