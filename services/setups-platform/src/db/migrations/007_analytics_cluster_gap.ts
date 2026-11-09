import { type Kysely, sql } from "kysely";

/**
 * cluster_gap_hours becomes a cache-key dimension (owner decision of
 * 2026-11-09): the analytics cluster gap is parameterized in the UI/API.
 * Stored as integer minutes with the -1 sentinel for "clustering off".
 * Existing clustered rows (maturity_tenths >= 0) were computed with the
 * former 8h constant, so they are backfilled with 480 minutes; primary
 * keys are rebuilt to include the new column.
 */
export async function up(db: Kysely<unknown>): Promise<void> {
  await db.schema.alterTable("analytics_matrix_cache")
    .addColumn("cluster_gap_minutes", "integer", (col) => col.notNull().defaultTo(-1))
    .execute();
  await db.schema.alterTable("analytics_trades_cache")
    .addColumn("cluster_gap_minutes", "integer", (col) => col.notNull().defaultTo(-1))
    .execute();
  await db.schema.alterTable("analytics_selection_cache")
    .addColumn("cluster_gap_minutes", "integer", (col) => col.notNull().defaultTo(-1))
    .execute();

  await sql`update analytics_matrix_cache set cluster_gap_minutes = 480 where maturity_tenths >= 0`.execute(db);
  await sql`update analytics_trades_cache set cluster_gap_minutes = 480 where maturity_tenths >= 0`.execute(db);
  await sql`update analytics_selection_cache set cluster_gap_minutes = 480 where maturity_tenths >= 0`.execute(db);

  await db.schema.alterTable("analytics_matrix_cache")
    .dropConstraint("analytics_matrix_cache_pk")
    .execute();
  await db.schema.alterTable("analytics_matrix_cache")
    .addPrimaryKeyConstraint("analytics_matrix_cache_pk", [
      "scope_key", "direction", "maturity_tenths", "cluster_gap_minutes", "month_key",
      "tp_pct", "sl_pct", "horizon_minutes", "time_shift_minutes",
      "price_change_ppm", "algorithm_version",
    ])
    .execute();

  await db.schema.alterTable("analytics_trades_cache")
    .dropConstraint("analytics_trades_cache_pk")
    .execute();
  await db.schema.alterTable("analytics_trades_cache")
    .addPrimaryKeyConstraint("analytics_trades_cache_pk", [
      "scope_key", "direction", "maturity_tenths", "cluster_gap_minutes", "exchange_code", "category",
      "symbol", "close_time_unix", "tp_pct", "sl_pct", "horizon_minutes",
      "time_shift_minutes", "price_change_ppm", "algorithm_version",
    ])
    .execute();

  await db.schema.alterTable("analytics_selection_cache")
    .dropConstraint("analytics_selection_cache_pk")
    .execute();
  await db.schema.alterTable("analytics_selection_cache")
    .addPrimaryKeyConstraint("analytics_selection_cache_pk", [
      "scope_key", "maturity_tenths", "cluster_gap_minutes", "month_key", "algorithm_version",
    ])
    .execute();
}

export async function down(db: Kysely<unknown>): Promise<void> {
  await db.schema.alterTable("analytics_selection_cache")
    .dropConstraint("analytics_selection_cache_pk")
    .execute();
  await db.schema.alterTable("analytics_selection_cache")
    .addPrimaryKeyConstraint("analytics_selection_cache_pk", [
      "scope_key", "maturity_tenths", "month_key", "algorithm_version",
    ])
    .execute();

  await db.schema.alterTable("analytics_trades_cache")
    .dropConstraint("analytics_trades_cache_pk")
    .execute();
  await db.schema.alterTable("analytics_trades_cache")
    .addPrimaryKeyConstraint("analytics_trades_cache_pk", [
      "scope_key", "direction", "maturity_tenths", "exchange_code", "category",
      "symbol", "close_time_unix", "tp_pct", "sl_pct", "horizon_minutes",
      "time_shift_minutes", "price_change_ppm", "algorithm_version",
    ])
    .execute();

  await db.schema.alterTable("analytics_matrix_cache")
    .dropConstraint("analytics_matrix_cache_pk")
    .execute();
  await db.schema.alterTable("analytics_matrix_cache")
    .addPrimaryKeyConstraint("analytics_matrix_cache_pk", [
      "scope_key", "direction", "maturity_tenths", "month_key",
      "tp_pct", "sl_pct", "horizon_minutes", "time_shift_minutes",
      "price_change_ppm", "algorithm_version",
    ])
    .execute();

  await db.schema.alterTable("analytics_selection_cache").dropColumn("cluster_gap_minutes").execute();
  await db.schema.alterTable("analytics_trades_cache").dropColumn("cluster_gap_minutes").execute();
  await db.schema.alterTable("analytics_matrix_cache").dropColumn("cluster_gap_minutes").execute();
}
