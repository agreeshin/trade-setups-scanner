import { sql, type Kysely } from "kysely";
import type { MarketDataDB } from "../db/schema.js";

export const INSERT_JOURNAL_LIMIT = 1024;
export interface ReadGuardToken { epoch: string; revision: string }
export interface ReadGuardInput {
  instrumentId: number;
  fromUnix: number;
  toUnix: number;
  previous: ReadGuardToken | null;
}
export interface ReadGuardResult {
  status: "valid" | "invalid" | "unknown";
  token: ReadGuardToken | null;
}

/** Caller holds the instrument watermark lock from BEFORE its canonical insert. */
export async function recordCanonicalInsert(
  db: Kysely<MarketDataDB>, instrumentId: number, times: Date[],
): Promise<void> {
  await recordCanonicalInserts(db, times.map((open_time) => ({ instrument_id: instrumentId, open_time })));
}

/** All corresponding watermark locks are held, in ascending instrument order. */
export async function recordCanonicalInserts(
  db: Kysely<MarketDataDB>, inserted: { instrument_id: number; open_time: Date }[],
): Promise<void> {
  if (inserted.length === 0) return;
  const ranges = new Map<number, { from: number; to: number }>();
  for (const row of inserted) {
    const at = row.open_time.getTime();
    const old = ranges.get(row.instrument_id);
    ranges.set(row.instrument_id, { from: Math.min(old?.from ?? at, at), to: Math.max(old?.to ?? at + 60_000, at + 60_000) });
  }
  const ids = [...ranges.keys()].sort((a, b) => a - b);
  const next = await sql<{ instrument_id: number; revision: string }>`UPDATE candle_insert_heads
    SET revision = revision + 1,
      pruned_through_revision = greatest(pruned_through_revision, revision + 1 - ${INSERT_JOURNAL_LIMIT})
    WHERE instrument_id = ANY(${ids}::int[]) RETURNING instrument_id, revision::text`.execute(db);
  if (next.rows.length !== ids.length) throw new Error("canonical insert metadata missing for batch");
  await db.insertInto("candle_insert_journal").values(next.rows.map((row) => {
    const range = ranges.get(row.instrument_id)!;
    return { instrument_id: row.instrument_id, revision: row.revision, from_ts: new Date(range.from), to_ts: new Date(range.to) };
  })).execute();
  if (next.rows.some((row) => BigInt(row.revision) > BigInt(INSERT_JOURNAL_LIMIT)))
    await sql`DELETE FROM candle_insert_journal j USING candle_insert_heads h
      WHERE j.instrument_id = h.instrument_id AND h.instrument_id = ANY(${ids}::int[])
        AND j.revision <= h.pruned_through_revision`.execute(db);
}

/** Must share the candle reader's REPEATABLE READ transaction/snapshot. */
export async function readGuards(
  db: Kysely<MarketDataDB>, inputs: ReadGuardInput[],
): Promise<Map<number, ReadGuardResult>> {
  const results = new Map<number, ReadGuardResult>();
  if (inputs.length === 0) return results;
  const heads = await sql<{
    instrumentId: number; revision: string; floor: string; ready: boolean;
    epoch: string; entries: string | null;
  }>`SELECT h.instrument_id AS "instrumentId", h.revision::text, h.pruned_through_revision::text AS floor,
      h.ready, s.epoch::text,
      CASE WHEN h.ready AND h.revision - h.pruned_through_revision BETWEEN 0 AND ${INSERT_JOURNAL_LIMIT}
        THEN (SELECT count(*)::text FROM candle_insert_journal j WHERE j.instrument_id = h.instrument_id
          AND j.revision > h.pruned_through_revision AND j.revision <= h.revision)
        ELSE NULL END AS entries
    FROM candle_insert_heads h CROSS JOIN candle_source_identity s
    WHERE s.id = 1 AND h.instrument_id = ANY(${inputs.map((input) => input.instrumentId)}::int[])`.execute(db);
  const byId = new Map(heads.rows.map((head) => [head.instrumentId, head]));
  const checks: { instrument_id: number; from_unix: number; to_unix: number; revision: string; head_revision: string }[] = [];
  for (const input of inputs) {
    const head = byId.get(input.instrumentId);
    const unavailable = { status: "unknown" as const, token: null };
    if (!head || !head.ready || head.entries === null
      || BigInt(head.entries) !== BigInt(head.revision) - BigInt(head.floor)) {
      results.set(input.instrumentId, unavailable); continue;
    }
    const token = { epoch: head.epoch, revision: head.revision };
    if (input.previous === null) { results.set(input.instrumentId, { status: "valid", token }); continue; }
    const previous = BigInt(input.previous.revision);
    if (input.previous.epoch !== head.epoch || previous < BigInt(head.floor) || previous > BigInt(head.revision)) {
      results.set(input.instrumentId, { status: "unknown", token }); continue;
    }
    results.set(input.instrumentId, { status: "valid", token });
    checks.push({ instrument_id: input.instrumentId, from_unix: input.fromUnix, to_unix: input.toUnix,
      revision: previous.toString(), head_revision: head.revision });
  }
  if (checks.length) {
    // Numeric Unix bounds need not fit JS Date/PostgreSQL timestamp: checking
    // the <=1024 retained ranges is bounded even for an arbitrarily long guard.
    const overlaps = await sql<{ instrumentId: number }>`
      SELECT g.instrument_id AS "instrumentId"
      FROM jsonb_to_recordset(${JSON.stringify(checks)}::jsonb) AS g(
        instrument_id integer, from_unix numeric, to_unix numeric, revision bigint, head_revision bigint)
      WHERE g.from_unix < g.to_unix AND EXISTS (
        SELECT 1 FROM candle_insert_journal j WHERE j.instrument_id = g.instrument_id
          AND j.revision > g.revision AND j.revision <= g.head_revision
          AND extract(epoch FROM j.from_ts) < g.to_unix AND extract(epoch FROM j.to_ts) > g.from_unix
      )`.execute(db);
    for (const row of overlaps.rows) results.get(row.instrumentId)!.status = "invalid";
  }
  return results;
}
