/* eslint-disable @typescript-eslint/no-unsafe-assignment, @typescript-eslint/no-unsafe-call, @typescript-eslint/no-unsafe-member-access --
   pg-boss types resolve in tsc but not in the ESLint project service. */
import PgBoss from "pg-boss";
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
  await boss.start();
  await boss.work(BACKFILL_QUEUE, async () => {
    await drainBackfills({ db: options.db, client });
  });
  const intervalMs = options.collectionIntervalMs ?? 30_000;
  const timer = setInterval(() => {
    void collectOnce(options.db, clock)
      .then(async (scheduled) => {
        if (scheduled > 0) {
          await boss.send({ name: BACKFILL_QUEUE, data: {} });
        }
      })
      .catch((error) => {
        console.error("continuous collection failed", error);
      });
  }, intervalMs);

  return {
    stop: async () => {
      clearInterval(timer);
      await boss.stop();
    },
  };
}
