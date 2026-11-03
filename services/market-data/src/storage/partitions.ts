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
    await sql`CREATE TABLE IF NOT EXISTS ${sql.table(name)} PARTITION OF candles
      FOR VALUES FROM (${sql.lit(cursor.toISOString())}) TO (${sql.lit(next.toISOString())})`.execute(
      db,
    );
    created.push(name);
    cursor = next;
  }
  return created;
}
