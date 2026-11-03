/* eslint-disable @typescript-eslint/no-unsafe-assignment, @typescript-eslint/no-unsafe-call, @typescript-eslint/no-unsafe-member-access --
   kysely/migration types resolve in tsc but not in the ESLint project service. */
import { afterAll, beforeAll, beforeEach, describe, expect, it } from "vitest";
import { execSync } from "node:child_process";
import { sql, type Kysely } from "kysely";
import { createDb } from "../../src/db/db.js";
import { createMigrator } from "../../src/db/migrator.js";
import type { MarketDataDB } from "../../src/db/schema.js";
import { ensureInstrument } from "../../src/storage/instruments.js";
import { insertCandles, readCandles, type CandleInsert } from "../../src/storage/candles.js";
import { getCoverage } from "../../src/storage/coverage.js";
import { BybitClient } from "../../src/ingest/bybit.js";
import { AdaptiveGate, DEFAULT_GATE_LIMITS } from "../../src/ingest/gate.js";
import { FakeClock } from "../../src/ingest/clock.js";
import type { Transport, TransportResponse } from "../../src/ingest/transport.js";
import { requestBackfill, runBackfillJobOnce } from "../../src/ingest/backfill.js";

const TEST_DATABASE_URL = process.env.TEST_DATABASE_URL;
const RESTORE_DB_NAME = "market_data_restore";
const RESTORE_DATABASE_URL = TEST_DATABASE_URL?.replace(/[^/]+$/, RESTORE_DB_NAME);
const CONTAINER = "trade-setups-dev-postgres";
const MINUTE = 60_000;
const T0 = Date.UTC(2024, 5, 1);
const minute = (offset: number): Date => new Date(T0 + offset * MINUTE);

function dockerExec(command: string, options?: { input?: string }): string {
  return execSync(`docker exec -i ${CONTAINER} ${command}`, {
    input: options?.input,
    encoding: "utf8",
    stdio: ["pipe", "pipe", "pipe"],
  });
}

function candle(openTime: Date, close: string): CandleInsert {
  return {
    openTime,
    open: close,
    high: close,
    low: close,
    close,
    volume: "1",
    quoteTurnover: "2",
  };
}

const rejectTransport: Transport = {
  fetch(): Promise<TransportResponse> {
    return Promise.reject(new Error("no exchange fetch expected after transfer"));
  },
};

describe.skipIf(!TEST_DATABASE_URL || process.env.CI === "true")(
  "market data transfer",
  () => {
    let db: Kysely<MarketDataDB>;
    let instrumentId: number;

    beforeAll(async () => {
      db = createDb(TEST_DATABASE_URL!);
      const migrator = createMigrator(db);
      const migrationResult = await migrator.migrateToLatest();
      if (migrationResult.error) {
        throw migrationResult.error;
      }
    });

    afterAll(async () => {
      await db.destroy();
      dockerExec(
        `psql -U trade_setups -d postgres -c "DROP DATABASE IF EXISTS ${RESTORE_DB_NAME}"`,
      );
    });

    beforeEach(async () => {
      await db.deleteFrom("collection_targets").execute();
      await db.deleteFrom("backfill_jobs").execute();
      await db.deleteFrom("service_state").execute();
      await db.deleteFrom("candle_conflicts").execute();
      await db.deleteFrom("candles").execute();
      await db.deleteFrom("instruments").execute();
      await db.deleteFrom("exchanges").execute();
      await sql`DROP TABLE IF EXISTS candles_2024_05, candles_2024_06, candles_2024_07 CASCADE`.execute(
        db,
      );
      instrumentId = await ensureInstrument(db, {
        exchangeCode: "bybit",
        category: "linear",
        symbol: "XFERUSDT",
      });
      await insertCandles(db, instrumentId, [
        candle(minute(0), "100"),
        candle(minute(1), "101"),
        candle(minute(5), "105"),
        candle(minute(6), "106"),
      ]);
      await db.insertInto("backfill_jobs").values({
        instrument_id: instrumentId,
        from_ts: minute(7),
        to_ts: minute(10),
        status: "queued",
        requested_by: ["client-a"],
      }).execute();
      await db.insertInto("service_state").values({
        key: "bybit_gate",
        value: { cooldownUntilIso: new Date(T0 + 3_600_000).toISOString() },
      }).execute();
    });

    it("restores a plain pg dump into a clean database identically", async () => {
      const dump = dockerExec(
        "pg_dump -U trade_setups --no-owner --no-privileges market_data_test",
      );
      dockerExec(
        `psql -U trade_setups -d postgres -c "DROP DATABASE IF EXISTS ${RESTORE_DB_NAME}"`,
      );
      dockerExec(`createdb -U trade_setups ${RESTORE_DB_NAME}`);
      dockerExec(
        `psql -U trade_setups -d ${RESTORE_DB_NAME} -v ON_ERROR_STOP=1`,
        { input: dump },
      );

      const restoreDb = createDb(RESTORE_DATABASE_URL!);
      try {
        const page = await readCandles(restoreDb, {
          instrumentId,
          from: minute(0),
          to: minute(7),
        });
        expect(page.candles.map((row) => row.close)).toEqual([
          "100",
          "101",
          "105",
          "106",
        ]);

        const coverage = await getCoverage(restoreDb, instrumentId);
        expect(coverage.intervals).toHaveLength(2);
        expect(coverage.gaps).toHaveLength(1);

        const instrument = await restoreDb
          .selectFrom("instruments")
          .innerJoin("exchanges", "exchanges.id", "instruments.exchange_id")
          .selectAll()
          .executeTakeFirstOrThrow();
        expect(instrument.symbol).toBe("XFERUSDT");

        const jobs = await restoreDb
          .selectFrom("backfill_jobs")
          .selectAll()
          .execute();
        expect(jobs).toHaveLength(1);
        expect(jobs[0]?.status).toBe("queued");

        const state = await restoreDb
          .selectFrom("service_state")
          .where("key", "=", "bybit_gate")
          .selectAll()
          .executeTakeFirstOrThrow();
        expect(state.value.cooldownUntilIso).not.toBeNull();
      } finally {
        await restoreDb.destroy();
      }
    });

    it("does not refetch stored history after transfer", async () => {
      const dump = dockerExec(
        "pg_dump -U trade_setups --no-owner --no-privileges market_data_test",
      );
      dockerExec(
        `psql -U trade_setups -d postgres -c "DROP DATABASE IF EXISTS ${RESTORE_DB_NAME}"`,
      );
      dockerExec(`createdb -U trade_setups ${RESTORE_DB_NAME}`);
      dockerExec(
        `psql -U trade_setups -d ${RESTORE_DB_NAME} -v ON_ERROR_STOP=1`,
        { input: dump },
      );

      const restoreDb = createDb(RESTORE_DATABASE_URL!);
      try {
        await restoreDb.deleteFrom("backfill_jobs").execute();
        await requestBackfill(restoreDb, {
          instrumentId,
          fromTs: minute(0),
          toTs: minute(1),
          requestedBy: "post-transfer",
        });
        const clock = new FakeClock(T0 + 2 * 1440 * MINUTE);
        const gate = new AdaptiveGate(
          { ...DEFAULT_GATE_LIMITS, initialIntervalMs: 1, minIntervalMs: 1 },
          clock,
        );
        const client = new BybitClient(rejectTransport, gate, clock);
        await runBackfillJobOnce({ db: restoreDb, client, chunkMinutes: 1440 });

        const jobs = await restoreDb
          .selectFrom("backfill_jobs")
          .selectAll()
          .execute();
        const covering = jobs.find((job) => job.to_ts.getTime() === minute(1).getTime());
        expect(covering?.status).toBe("done");
        const page = await readCandles(restoreDb, {
          instrumentId,
          from: minute(0),
          to: minute(7),
        });
        expect(page.candles).toHaveLength(4);
      } finally {
        await restoreDb.destroy();
      }
    });
  },
);
