import { sql, type Kysely } from "kysely";
import type { MarketDataDB } from "../db/schema.js";

export function monthStart(date: Date): Date {
  return new Date(Date.UTC(date.getUTCFullYear(), date.getUTCMonth(), 1));
}

export function partitionName(month: Date): string {
  return `candles_${month.getUTCFullYear()}_${String(month.getUTCMonth() + 1).padStart(2, "0")}`;
}

export async function ensureMonthPartitions(
  db: Kysely<MarketDataDB>,
  from: Date,
  to: Date,
): Promise<string[]> {
  const created: string[] = [];
  let cursor = monthStart(from);
  const end = monthStart(to);
  while (cursor <= end) {
    const next = new Date(Date.UTC(cursor.getUTCFullYear(), cursor.getUTCMonth() + 1, 1));
    const name = partitionName(cursor);
    // Bind parameters are not allowed in DDL, so bounds are embedded as
    // escaped literals via sql.lit.
    const exists = await sql<{ name: string | null }>`SELECT to_regclass(${name})::text AS name`.execute(db);
    if (exists.rows[0]?.name) { cursor = next; continue; }
    const create = async (tx: Kysely<MarketDataDB>) => {
    await sql`SELECT pg_advisory_xact_lock(72423414)`.execute(tx);
    await sql`CREATE TABLE IF NOT EXISTS ${sql.table(name)} PARTITION OF candles
      FOR VALUES FROM (${sql.lit(cursor.toISOString())}) TO (${sql.lit(next.toISOString())})`.execute(
      tx,
    );
    };
    if (db.isTransaction) await create(db);
    else await db.transaction().execute(create);
    created.push(name);
    cursor = next;
  }
  return created;
}
