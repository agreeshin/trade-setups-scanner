import { Worker } from "node:worker_threads";
import { sql, type Kysely, type Selectable } from "kysely";
import type { ReplayRunsTable, SignalsDB } from "../db/schema.js";
import type { SpikeParams, SpikeSetup } from "../detector/price-volume-spike.js";
import { SPIKE_ALGORITHM_ID, SPIKE_ALGORITHM_VERSION } from "../detector/price-volume-spike.js";
import { fetchCatalog } from "../platform-catalog.js";
import { getSetting } from "../settings.js";
import { ApiCandleSource, type CandleSource, type CandleBarDto } from "./source.js";
import { cacheGeneration, canonicalCacheKey, lockReplayCache, type CacheQuery } from "./cache.js";
import { ReplayQualityTracker, saveQualityPage, type ReplayQuality, type QualityInterval } from "./quality.js";

type Run = Selectable<ReplayRunsTable>;
export const MAX_PENDING_REPLAYS = 100;
const pending = ["queued", "running", "cancel_requested"];

export function replayRunDto(row: Run) {
  return { id: row.id, exchange: row.exchange_code, category: row.category, symbol: row.symbol,
    status: row.status, cacheHit: row.cache_hit, setupCount: row.setups?.length ?? 0,
    quality: row.quality, qualitySourceRunId: row.quality_source_run_id,
    qualityComputedAt: row.quality_computed_at?.toISOString() ?? null,
    warmupMinutes: row.warmup_minutes, processedMinutes: row.processed_minutes, error: row.error,
    algorithmId: row.algorithm_id, algorithmVersion: row.algorithm_version, params: row.params,
    fromUnix: row.from_ts.getTime() / 1000, toUnix: row.to_ts.getTime() / 1000,
    createdAt: row.created_at.toISOString() };
}

export async function enqueueReplayRuns(db: Kysely<SignalsDB>, input: {
  instruments: { exchange: string; category: string; symbol: string }[];
  fromUnix: number; toUnix: number; params: SpikeParams;
}): Promise<Run[] | null> {
  return db.transaction().execute(async (tx) => {
    await sql`select pg_advisory_xact_lock(72423412)`.execute(tx);
    const count = await tx.selectFrom("replay_runs").select((eb) => eb.fn.countAll<string>().as("n"))
      .where("status", "in", pending).executeTakeFirstOrThrow();
    if (Number(count.n) + input.instruments.length > MAX_PENDING_REPLAYS) return null;
    return tx.insertInto("replay_runs").values(input.instruments.map((instrument) => ({
      exchange_code: instrument.exchange, category: instrument.category, symbol: instrument.symbol,
      from_ts: new Date(input.fromUnix * 1000), to_ts: new Date(input.toUnix * 1000),
      algorithm_id: SPIKE_ALGORITHM_ID, algorithm_version: SPIKE_ALGORITHM_VERSION,
      params: input.params, status: "queued", cache_hit: false,
    }))).returningAll().execute();
  });
}

export async function cancelReplayRun(db: Kysely<SignalsDB>, id: number): Promise<Run | undefined> {
  return db.transaction().execute(async (tx) => {
    const run = await tx.selectFrom("replay_runs").selectAll().where("id", "=", id).forUpdate().executeTakeFirst();
    if (!run || !pending.includes(run.status)) return run;
    return tx.updateTable("replay_runs").set({ status: run.status === "queued" ? "cancelled" : "cancel_requested", updated_at: new Date() })
      .where("id", "=", id).returningAll().executeTakeFirstOrThrow();
  });
}

/** One runtime per application process; stop before destroying the DB pool. */
export async function startReplayJobs(db: Kysely<SignalsDB>, marketData: { url: string; apiKey: string }, options: {
  pollIntervalMs?: number;
  onError?: (error: unknown) => void;
  sourceForRun?: (run: Run) => Promise<CandleSource>;
} = {}): Promise<{ stop(): Promise<void> }> {
  // Explicit interruption preserves partial evidence without pretending to resume detector state.
  await db.updateTable("replay_runs").set({ status: "failed", error: "interrupted by application restart", updated_at: new Date() })
    .where("status", "=", "running").execute();
  await db.updateTable("replay_runs").set({ status: "cancelled", updated_at: new Date() })
    .where("status", "=", "cancel_requested").execute();
  let stopped = false;
  let timer: ReturnType<typeof setTimeout> | undefined;
  let wake: (() => void) | undefined;
  const wait = () => new Promise<void>((resolve) => {
    wake = resolve;
    timer = setTimeout(resolve, options.pollIntervalMs ?? 500);
    timer.unref();
  });

  async function execute(run: Run) {
    let worker: Worker | undefined;
    try {
      const generation = await cacheGeneration(db);
      const query: CacheQuery = { exchangeCode: run.exchange_code, category: run.category, symbol: run.symbol,
        algorithmId: run.algorithm_id, algorithmVersion: run.algorithm_version, params: run.params,
        fromUnix: run.from_ts.getTime() / 1000, toUnix: run.to_ts.getTime() / 1000 };
      const cacheKey = canonicalCacheKey(query);
      const cacheHit = await db.transaction().execute(async (tx) => {
        await lockReplayCache(tx);
        const cached = await tx.selectFrom("replay_cache").select(["warmup_minutes", "processed_minutes", "quality", "quality_source_run_id", "computed_at"])
          .where("cache_key", "=", cacheKey).executeTakeFirst();
        if (!cached) return false;
        await tx.updateTable("replay_runs").set({ status: "done", cache_hit: true,
          quality: cached.quality, quality_source_run_id: cached.quality_source_run_id, quality_computed_at: cached.computed_at,
          setups: sql<SpikeSetup[]>`(select setups from replay_cache where cache_key = ${cacheKey})`,
          warmup_minutes: cached.warmup_minutes, processed_minutes: cached.processed_minutes, updated_at: new Date() })
          .where("id", "=", run.id).where("status", "=", "running").execute();
        return true;
      });
      if (cacheHit) return;
      if (run.algorithm_id !== SPIKE_ALGORITHM_ID || run.algorithm_version !== SPIKE_ALGORITHM_VERSION) {
        throw new Error("unsupported_algorithm_version: create a new run using the current algorithm");
      }
      await saveQualityPage(db, run.id, new ReplayQualityTracker(query.fromUnix, query.toUnix,
        run.params.baselineLongDays * 1440).quality, []);
      let source: CandleSource;
      if (options.sourceForRun) source = await options.sourceForRun(run);
      else {
        const catalog = await fetchCatalog(marketData.url, marketData.apiKey);
        const instrument = catalog.find((item) => item.exchange === run.exchange_code && item.category === run.category && item.symbol === run.symbol);
        if (!instrument) throw new Error("instrument not found in market data catalog");
        source = new ApiCandleSource(marketData.url, marketData.apiKey, instrument.id);
      }
      const isTs = import.meta.url.endsWith(".ts");
      worker = new Worker(new URL(isTs ? "./job-worker.ts" : "./job-worker.js", import.meta.url), {
        workerData: { params: run.params, fromUnix: query.fromUnix, toUnix: query.toUnix },
        ...(isTs ? { execArgv: ["--import", "tsx"] } : {}),
      });
      let workerError: Error | undefined;
      worker.on("error", (error) => { workerError = error instanceof Error ? error : new Error(String(error)); });
      worker.on("exit", (code) => { workerError ??= new Error(`replay worker exited (${code})`); });
      interface PageResult { setups: SpikeSetup[]; warmupMinutes: number; processedMinutes: number; quality: ReplayQuality; intervals: QualityInterval[] }
      const processPage = (bars: CandleBarDto[], finished = false) => new Promise<PageResult>((resolve, reject) => {
        if (workerError) { reject(workerError); return; }
        const cleanup = () => { worker!.off("message", message); worker!.off("error", error); worker!.off("exit", exit); };
        const message = (value: PageResult) => { cleanup(); resolve(value); };
        const error = (value: Error) => { cleanup(); reject(value); };
        const exit = (code: number) => error(new Error(`replay worker exited (${code})`));
        worker!.once("message", message); worker!.once("error", error); worker!.once("exit", exit);
      worker!.postMessage({ bars, finished });
      });
      let cursor: number | undefined;
      let warmupMinutes = 0;
      let processedMinutes = 0;
      for (;;) {
        const state = await db.selectFrom("replay_runs").select("status").where("id", "=", run.id).executeTakeFirstOrThrow();
        if (state.status === "cancel_requested" || stopped) return;
        if ((await getSetting(db, "replay")).paused === true) { await wait(); continue; }
        const page = await source.read({ fromUnix: query.fromUnix - run.params.baselineLongDays * 86400,
          toUnix: query.toUnix, afterUnix: cursor, limit: 5000 });
        if (page.bars.length === 0) break;
        if (page.bars.length > 5000) throw new Error("replay source exceeded page limit");
        const result = await processPage(page.bars);
        warmupMinutes += result.warmupMinutes;
        processedMinutes += result.processedMinutes;
        await db.transaction().execute(async (tx) => {
        await saveQualityPage(tx, run.id, result.quality, result.intervals);
        await tx.updateTable("replay_runs").set({
          setups: sql<SpikeSetup[]>`coalesce(setups, '[]'::jsonb) || ${JSON.stringify(result.setups)}::jsonb`,
          warmup_minutes: warmupMinutes, processed_minutes: processedMinutes, updated_at: new Date(),
        }).where("id", "=", run.id).where("status", "in", ["running", "cancel_requested"]).execute();
        });
        if (page.nextCursorUnix === null) break;
        if (page.nextCursorUnix <= (cursor ?? Number.NEGATIVE_INFINITY)) throw new Error("replay source cursor did not advance");
        cursor = page.nextCursorUnix;
      }
      const report = await processPage([], true);
      await db.transaction().execute(async (tx) => {
        const final = await tx.selectFrom("replay_runs").select("status").where("id", "=", run.id).forUpdate().executeTakeFirstOrThrow();
        if (final.status !== "running" || stopped) return;
        await saveQualityPage(tx, run.id, report.quality, report.intervals);
        const computedAt = new Date();
        await tx.updateTable("replay_runs").set({ status: "done", quality_computed_at: computedAt, updated_at: computedAt }).where("id", "=", run.id).execute();
        await lockReplayCache(tx);
        if (await cacheGeneration(tx) === generation) {
          // A savepoint keeps a cache disk/quota failure from destroying the completed result.
          await sql`savepoint replay_cache_publish`.execute(tx);
          try {
            await sql`insert into replay_cache
              (cache_key, exchange_code, category, symbol, algorithm_id, algorithm_version, params, from_ts, to_ts, setups, warmup_minutes, processed_minutes, quality, quality_source_run_id, computed_at)
              select ${cacheKey}, exchange_code, category, symbol, algorithm_id, algorithm_version, params, from_ts, to_ts,
                coalesce(setups, '[]'::jsonb), ${warmupMinutes}, ${processedMinutes}, quality, quality_source_run_id, ${computedAt}
              from replay_runs where id = ${run.id}
              on conflict (cache_key) do update set setups = excluded.setups,
                warmup_minutes = excluded.warmup_minutes, processed_minutes = excluded.processed_minutes,
                quality = excluded.quality, quality_source_run_id = excluded.quality_source_run_id, computed_at = excluded.computed_at`.execute(tx);
          } catch (error) {
            await sql`rollback to savepoint replay_cache_publish`.execute(tx);
            await tx.updateTable("replay_runs").set({ error: `cache write failed: ${String(error).slice(0, 300)}` }).where("id", "=", run.id).execute();
          }
        }
      });
    } catch (error) {
      await db.updateTable("replay_runs").set({ status: "failed", error: String(error).slice(0, 500), updated_at: new Date() })
        .where("id", "=", run.id).where("status", "=", "running").execute();
      options.onError?.(error);
    } finally {
      if (worker) await worker.terminate();
      await db.updateTable("replay_runs").set({ status: "cancelled", updated_at: new Date() })
        .where("id", "=", run.id).where("status", "=", "cancel_requested").execute();
      if (stopped) await db.updateTable("replay_runs").set({ status: "failed", error: "interrupted by application shutdown", updated_at: new Date() })
        .where("id", "=", run.id).where("status", "=", "running").execute();
    }
  }
  const loop = (async () => {
    while (!stopped) {
      try {
        if ((await getSetting(db, "replay")).paused !== true) {
          const run = await db.transaction().execute(async (tx) => {
            const row = await tx.selectFrom("replay_runs").selectAll().where("status", "=", "queued").orderBy("id").limit(1).forUpdate().skipLocked().executeTakeFirst();
            if (!row) return undefined;
            return tx.updateTable("replay_runs").set({ status: "running", updated_at: new Date() }).where("id", "=", row.id).returningAll().executeTakeFirstOrThrow();
          });
          if (run) { await execute(run); continue; }
        }
      } catch (error) { options.onError?.(error); }
      if (!stopped) await wait();
    }
  })();
  return { async stop() { stopped = true; if (timer) clearTimeout(timer); wake?.(); await loop; } };
}
