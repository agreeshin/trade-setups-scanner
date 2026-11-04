 
import { PgBoss } from "pg-boss";
import type { Kysely } from "kysely";
import type { MarketDataDB } from "../db/schema.js";
import { AdaptiveGate, DEFAULT_GATE_LIMITS } from "./gate.js";
import { createGatePersistence } from "./state-persistence.js";
import { BybitClient } from "./bybit.js";
import { FetchTransport } from "./transport.js";
import { RealClock, type Clock } from "./clock.js";
import { collectOnce, drainBackfills } from "./continuous.js";

const BACKFILL_QUEUE = "backfill";

export interface IngestRuntimeOptions {
  db: Kysely<MarketDataDB>;
  databaseUrl: string;
  clock?: Clock;
  collectionIntervalMs?: number;
}

export interface IngestRuntime {
  stop(): Promise<void>;
}

export async function startIngestRuntime(
  options: IngestRuntimeOptions,
): Promise<IngestRuntime> {
  const clock = options.clock ?? new RealClock();
  const gate = new AdaptiveGate(
    DEFAULT_GATE_LIMITS,
    clock,
    createGatePersistence(options.db),
  );
  const client = new BybitClient(new FetchTransport(), gate, clock);
  const boss = new PgBoss({ connectionString: options.databaseUrl });
  boss.on("error", (error: unknown) => {
    console.error("pg-boss error", error);
  });
  await boss.start();
  await boss.createQueue(BACKFILL_QUEUE);
  await boss.work(BACKFILL_QUEUE, async () => {
    await drainBackfills({ db: options.db, client });
  });
  const intervalMs = options.collectionIntervalMs ?? 30_000;
  const timer = setInterval(() => {
    void (async () => {
      try {
        const scheduled = await collectOnce(options.db, clock);
        const pending = await options.db
          .selectFrom("backfill_jobs")
          .where("status", "=", "queued")
          .limit(1)
          .select("id")
          .executeTakeFirst();
        if (scheduled > 0 || pending !== undefined) {
          await boss.send({ name: BACKFILL_QUEUE, data: {} });
        }
      } catch (error) {
        console.error("continuous collection failed", error);
      }
    })();
  }, intervalMs);

  return {
    stop: async () => {
      clearInterval(timer);
      await boss.stop();
    },
  };
}
