import type { Kysely } from "kysely";
import type { MarketDataDB } from "../db/schema.js";
import { AdaptiveGate, DEFAULT_GATE_LIMITS } from "./gate.js";
import { createGatePersistence } from "./state-persistence.js";
import { BybitClient } from "./bybit.js";
import { FetchTransport } from "./transport.js";
import { RealClock, type Clock } from "./clock.js";
import { collectOnce, drainBackfills } from "./continuous.js";
import { topUpBackfillPlans } from "./plans.js";
import { drainGapRepairs } from "./gap-repairs.js";
import type { BackfillAdmissionLimits } from "./backfill.js";
import { FreshCollector, type FreshOptions } from "./fresh.js";

export interface IngestRuntimeOptions {
  db: Kysely<MarketDataDB>;
  databaseUrl: string;
  client?: BybitClient;
  clock?: Clock;
  collectionIntervalMs?: number;
  backfillLimits?: BackfillAdmissionLimits;
  fresh?: FreshOptions;
  onEvent?: (event: string, detail: string) => void;
}
export interface IngestRuntime { stop(): Promise<void> }

export async function startIngestRuntime(options: IngestRuntimeOptions): Promise<IngestRuntime> {
  const clock = options.clock ?? new RealClock();
  const client = options.client ?? new BybitClient(new FetchTransport(),
    new AdaptiveGate(DEFAULT_GATE_LIMITS, clock, createGatePersistence(options.db)), clock);
  const lifetime = new AbortController();
  const log = options.onEvent ?? ((event: string, detail: string) => console.info(event, detail));
  const error = (event: string, value: unknown) => log(event, (value instanceof Error ? value.message : String(value)).slice(0, 200));
  const fresh = new FreshCollector(options.db, client, clock, options.fresh, log);
  // One runtime owns the service's historical queue. Do not multiply drains:
  // legacy backfill claims are not atomic across independent worker processes.
  await options.db.updateTable("backfill_jobs").set({ status: "queued" }).where("status", "=", "running").execute();
  let history: Promise<void> | undefined;
  let pulse: Promise<void> | undefined;
  const startHistory = () => {
    if (history || lifetime.signal.aborted) return;
    history = (async () => {
      await drainBackfills({ db: options.db, client, signal: lifetime.signal });
      if (!lifetime.signal.aborted) await drainGapRepairs({ db: options.db, client, signal: lifetime.signal });
    })().catch((value: unknown) => { if (!lifetime.signal.aborted) error("history_drain_failed", value); })
      .finally(() => { history = undefined; });
  };
  const tick = (): Promise<void> => {
    if (pulse) return pulse;
    if (lifetime.signal.aborted) return Promise.resolve();
    pulse = (async () => {
      await collectOnce(options.db, clock, options.backfillLimits);
      await topUpBackfillPlans(options.db, options.backfillLimits);
      startHistory();
    })().catch((value: unknown) => { if (!lifetime.signal.aborted) error("history_schedule_failed", value); startHistory(); })
      .finally(() => { pulse = undefined; });
    return pulse;
  };
  try { await fresh.start(); } catch (value) { await fresh.stop(); client.close(); throw value; }
  const timer = setInterval(() => { void tick(); }, options.collectionIntervalMs ?? 30_000);
  void tick();
  log("ingest_started", "independent WS/fresh and serial historical lanes");
  return { stop: async () => {
    clearInterval(timer); lifetime.abort(); client.close();
    await fresh.stop(); await pulse; await history;
    log("ingest_stopped", "requests, sockets and workers drained");
  } };
}
