import { type Kysely, sql } from "kysely";
import type { SignalsDB } from "../db/schema.js";
import { listReplayBatches, replayBatchResultPage } from "../replay/batches.js";
import { getAlgorithm } from "../algorithms.js";
import type { AnalyticsSetup } from "./engine.js";

export type SetKind = "batch" | "run";

export interface SetRef {
  kind: SetKind;
  id: number;
}

export interface AnalyticsSetInfo {
  setId: string;
  kind: SetKind;
  numericId: number;
  /** Selector label like `#4 - Всплеск цены и оборота · 1.1.1`. */
  label: string;
  algorithmId: string;
  algorithmVersion: string | null;
  exchange: string;
  category: string;
  symbol: string | null;
  setupCount: number;
  fromUnix: number | null;
  toUnix: number | null;
  createdAt: string;
}

export function parseSetId(setId: string): SetRef | null {
  const match = /^(batch|run)-(\d+)$/.exec(setId);
  if (match === null) return null;
  const id = Number(match[2]);
  if (!Number.isSafeInteger(id) || id < 1) return null;
  return { kind: match[1] as SetKind, id };
}

function setIdOf(ref: SetRef): string {
  return `${ref.kind}-${ref.id}`;
}

async function algorithmSuffix(db: Kysely<SignalsDB>, algorithmId: string, version: string | null): Promise<string> {
  const algorithm = await getAlgorithm(db, algorithmId).catch(() => undefined);
  const name = algorithm?.name ?? algorithmId;
  return version === null ? `- ${name}` : `- ${name} · ${version}`;
}

/** Finished historical launches available as analytics setup sets. */
export async function listAnalyticsSets(db: Kysely<SignalsDB>): Promise<AnalyticsSetInfo[]> {
  const batches = (await listReplayBatches(db, 200)).filter((batch) => batch.status === "done");
  const standaloneRuns = await db
    .selectFrom("replay_runs")
    .select(["id", "exchange_code", "category", "symbol", "algorithm_id", "algorithm_version", "from_ts", "to_ts", "created_at", "setups"])
    .where("batch_id", "is", null)
    .where("status", "=", "done")
    .orderBy("id", "desc")
    .limit(200)
    .execute();

  const nameCache = new Map<string, string>();
  const labelFor = async (numericId: number, algorithmId: string, version: string | null): Promise<string> => {
    const key = `${algorithmId}@${version ?? ""}`;
    let suffix = nameCache.get(key);
    if (suffix === undefined) {
      suffix = await algorithmSuffix(db, algorithmId, version);
      nameCache.set(key, suffix);
    }
    return `#${numericId} ${suffix}`;
  };

  const sets: AnalyticsSetInfo[] = [];
  for (const batch of batches) {
    sets.push({
      setId: setIdOf({ kind: "batch", id: batch.id }),
      kind: "batch",
      numericId: batch.id,
      label: await labelFor(batch.id, batch.algorithm_id, batch.algorithm_version),
      algorithmId: batch.algorithm_id,
      algorithmVersion: batch.algorithm_version,
      exchange: batch.exchange_code,
      category: batch.category,
      symbol: null,
      setupCount: batch.setup_count,
      fromUnix: Math.floor(batch.from_ts.getTime() / 1000),
      toUnix: Math.floor(batch.to_ts.getTime() / 1000),
      createdAt: batch.created_at.toISOString(),
    });
  }
  for (const run of standaloneRuns) {
    const setups = Array.isArray(run.setups) ? run.setups.length : 0;
    sets.push({
      setId: setIdOf({ kind: "run", id: run.id }),
      kind: "run",
      numericId: run.id,
      label: await labelFor(run.id, run.algorithm_id, run.algorithm_version),
      algorithmId: run.algorithm_id,
      algorithmVersion: run.algorithm_version,
      exchange: run.exchange_code,
      category: run.category,
      symbol: run.symbol,
      setupCount: setups,
      fromUnix: Math.floor(run.from_ts.getTime() / 1000),
      toUnix: Math.floor(run.to_ts.getTime() / 1000),
      createdAt: run.created_at.toISOString(),
    });
  }
  sets.sort((a, b) => b.createdAt.localeCompare(a.createdAt) || a.setId.localeCompare(b.setId));
  return sets;
}

/** Load all setups of a set; undefined when the set does not exist. */
export async function loadAnalyticsSetups(db: Kysely<SignalsDB>, ref: SetRef): Promise<AnalyticsSetup[] | undefined> {
  if (ref.kind === "run") {
    const run = await db
      .selectFrom("replay_runs")
      .select(["setups", "exchange_code", "category", "symbol"])
      .where("id", "=", ref.id)
      .executeTakeFirst();
    if (run === undefined || !Array.isArray(run.setups)) return undefined;
    return run.setups.map((setup) => ({
      exchangeCode: run.exchange_code,
      category: run.category,
      symbol: run.symbol,
      openTimeUnix: setup.openTimeUnix,
      closeTimeUnix: setup.closeTimeUnix,
      closePrice: setup.closePrice,
    }));
  }

  const first = await replayBatchResultPage(db, ref.id, 0, 1);
  if (first === undefined) return undefined;
  const total = first.total;
  const rows = total <= 1 ? first.setups : [...first.setups];
  for (let offset = rows.length; offset < total; offset += 500) {
    const page = await replayBatchResultPage(db, ref.id, offset, Math.min(500, total - offset));
    if (page === undefined) return undefined;
    rows.push(...page.setups);
  }
  const setups: AnalyticsSetup[] = [];
  const seen = new Set<string>();
  for (const row of rows) {
    const identity = `${row.exchange_code}\u0000${row.category}\u0000${row.symbol}\u0000${row.setup.closeTimeUnix}`;
    if (seen.has(identity)) continue;
    seen.add(identity);
    setups.push({
      exchangeCode: row.exchange_code,
      category: row.category,
      symbol: row.symbol,
      openTimeUnix: row.setup.openTimeUnix,
      closeTimeUnix: row.setup.closeTimeUnix,
      closePrice: row.setup.closePrice,
    });
  }
  setups.sort((a, b) => a.closeTimeUnix - b.closeTimeUnix);
  return setups;
}

/** Selection-scope fingerprint of a setup list (candle-independent). */
export function setupsFingerprint(setups: AnalyticsSetup[]): { count: number; first: number | null; last: number | null } {
  return {
    count: setups.length,
    first: setups[0]?.closeTimeUnix ?? null,
    last: setups[setups.length - 1]?.closeTimeUnix ?? null,
  };
}

export async function analyticsTablesExist(db: Kysely<SignalsDB>): Promise<boolean> {
  const result = await sql<{ exists: boolean }>`select exists (
    select 1 from information_schema.tables where table_name = 'analytics_matrix_cache'
  ) as exists`.execute(db);
  return result.rows[0]?.exists === true;
}
