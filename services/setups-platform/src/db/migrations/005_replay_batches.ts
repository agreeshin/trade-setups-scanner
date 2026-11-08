import { type Kysely, sql } from "kysely";

export async function up(db: Kysely<unknown>): Promise<void> {
  await db.schema.createTable("replay_batches")
    .addColumn("id", "serial", (col) => col.primaryKey())
    .addColumn("exchange_code", "text", (col) => col.notNull())
    .addColumn("category", "text", (col) => col.notNull())
    .addColumn("algorithm_id", "text", (col) => col.notNull())
    .addColumn("algorithm_version", "text")
    .addColumn("params", "jsonb", (col) => col.notNull())
    .addColumn("from_ts", "timestamptz", (col) => col.notNull())
    .addColumn("to_ts", "timestamptz", (col) => col.notNull())
    .addColumn("instrument_count", "integer", (col) => col.notNull())
    .addColumn("created_at", "timestamptz", (col) => col.notNull().defaultTo(sql`now()`))
    .execute();
  await db.schema.alterTable("replay_runs")
    .addColumn("batch_id", "integer", (col) => col.references("replay_batches.id").onDelete("cascade"))
    .execute();
  await db.schema.createIndex("replay_runs_batch_id_idx").on("replay_runs").column("batch_id").execute();
}

export async function down(db: Kysely<unknown>): Promise<void> {
  await db.schema.alterTable("replay_runs").dropColumn("batch_id").execute();
  await db.schema.dropTable("replay_batches").execute();
}
