import { type Kysely, sql } from "kysely";

export async function up(db: Kysely<unknown>): Promise<void> {
  for (const table of ["replay_runs", "replay_cache"]) {
    await db.schema.alterTable(table).addColumn("quality", "jsonb")
      .addColumn("quality_source_run_id", "integer", (col) => col.references("replay_runs.id").onDelete("set null"))
      .execute();
  }
  await db.schema.alterTable("replay_runs").addColumn("quality_computed_at", "timestamptz").execute();
  await db.schema.createTable("replay_quality_intervals")
    .addColumn("run_id", "integer", (col) => col.notNull().references("replay_runs.id").onDelete("cascade"))
    .addColumn("from_unix", "bigint", (col) => col.notNull())
    .addColumn("to_unix", "bigint", (col) => col.notNull())
    .addColumn("reason", "text", (col) => col.notNull())
    .addPrimaryKeyConstraint("replay_quality_intervals_pk", ["run_id", "from_unix"])
    .addCheckConstraint("replay_quality_positive_interval", sql`to_unix > from_unix`).execute();
  for (const table of ["replay_runs", "replay_cache"]) {
    await db.schema.createIndex(`${table}_quality_source_idx`).on(table).column("quality_source_run_id").execute();
  }
}

export async function down(db: Kysely<unknown>): Promise<void> {
  await db.schema.dropTable("replay_quality_intervals").execute();
  await db.schema.alterTable("replay_runs").dropColumn("quality_computed_at").execute();
  for (const table of ["replay_cache", "replay_runs"]) {
    await db.schema.alterTable(table).dropColumn("quality_source_run_id").dropColumn("quality").execute();
  }
}
