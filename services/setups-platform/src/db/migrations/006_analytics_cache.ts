import { type Kysely, sql } from "kysely";

/**
 * Cache tables for the analytics engine. Results are stored in the
 * platform (signals) DB next to user data; market history is never
 * touched. `-1` sentinels encode "parameter not set" for nullable
 * numeric cache-key dimensions (maturity tenths, price-change ppm).
 */
export async function up(db: Kysely<unknown>): Promise<void> {
  await db.schema.createTable("analytics_matrix_cache")
    .addColumn("scope_key", "text", (col) => col.notNull())
    .addColumn("direction", "text", (col) => col.notNull())
    .addColumn("maturity_tenths", "integer", (col) => col.notNull())
    .addColumn("month_key", "text", (col) => col.notNull())
    .addColumn("tp_pct", "numeric", (col) => col.notNull())
    .addColumn("sl_pct", "numeric", (col) => col.notNull())
    .addColumn("horizon_minutes", "integer", (col) => col.notNull())
    .addColumn("time_shift_minutes", "integer", (col) => col.notNull())
    .addColumn("price_change_ppm", "integer", (col) => col.notNull())
    .addColumn("algorithm_version", "integer", (col) => col.notNull())
    .addColumn("result_json", "jsonb", (col) => col.notNull())
    .addColumn("created_at", "timestamptz", (col) => col.notNull().defaultTo(sql`now()`))
    .addPrimaryKeyConstraint("analytics_matrix_cache_pk", [
      "scope_key", "direction", "maturity_tenths", "month_key",
      "tp_pct", "sl_pct", "horizon_minutes", "time_shift_minutes",
      "price_change_ppm", "algorithm_version",
    ])
    .execute();

  await db.schema.createTable("analytics_trades_cache")
    .addColumn("scope_key", "text", (col) => col.notNull())
    .addColumn("direction", "text", (col) => col.notNull())
    .addColumn("maturity_tenths", "integer", (col) => col.notNull())
    .addColumn("exchange_code", "text", (col) => col.notNull())
    .addColumn("category", "text", (col) => col.notNull())
    .addColumn("symbol", "text", (col) => col.notNull())
    .addColumn("close_time_unix", "bigint", (col) => col.notNull())
    .addColumn("tp_pct", "numeric", (col) => col.notNull())
    .addColumn("sl_pct", "numeric", (col) => col.notNull())
    .addColumn("horizon_minutes", "integer", (col) => col.notNull())
    .addColumn("time_shift_minutes", "integer", (col) => col.notNull())
    .addColumn("price_change_ppm", "integer", (col) => col.notNull())
    .addColumn("algorithm_version", "integer", (col) => col.notNull())
    .addColumn("result_json", "jsonb", (col) => col.notNull())
    .addColumn("created_at", "timestamptz", (col) => col.notNull().defaultTo(sql`now()`))
    .addPrimaryKeyConstraint("analytics_trades_cache_pk", [
      "scope_key", "direction", "maturity_tenths", "exchange_code", "category",
      "symbol", "close_time_unix", "tp_pct", "sl_pct", "horizon_minutes",
      "time_shift_minutes", "price_change_ppm", "algorithm_version",
    ])
    .execute();

  await db.schema.createTable("analytics_selection_cache")
    .addColumn("scope_key", "text", (col) => col.notNull())
    .addColumn("maturity_tenths", "integer", (col) => col.notNull())
    .addColumn("month_key", "text", (col) => col.notNull())
    .addColumn("algorithm_version", "integer", (col) => col.notNull())
    .addColumn("result_json", "jsonb", (col) => col.notNull())
    .addColumn("created_at", "timestamptz", (col) => col.notNull().defaultTo(sql`now()`))
    .addPrimaryKeyConstraint("analytics_selection_cache_pk", [
      "scope_key", "maturity_tenths", "month_key", "algorithm_version",
    ])
    .execute();
}

export async function down(db: Kysely<unknown>): Promise<void> {
  await db.schema.dropTable("analytics_selection_cache").ifExists().execute();
  await db.schema.dropTable("analytics_trades_cache").ifExists().execute();
  await db.schema.dropTable("analytics_matrix_cache").ifExists().execute();
}
