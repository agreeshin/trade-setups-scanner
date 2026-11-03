/* eslint-disable @typescript-eslint/no-explicit-any -- official Kysely migration
   signature: migrations create the schema, so they use a permissive db view. */
import { sql, type Kysely } from "kysely";

export async function up(db: Kysely<any>): Promise<void> {
  await db.schema
    .createTable("service_state")
    .addColumn("key", "text", (col) => col.primaryKey())
    .addColumn("value", "jsonb", (col) => col.notNull())
    .addColumn("updated_at", "timestamptz", (col) => col.notNull().defaultTo(sql`now()`))
    .execute();

  await db.schema
    .createTable("backfill_jobs")
    .addColumn("id", "integer", (col) => col.generatedAlwaysAsIdentity().primaryKey())
    .addColumn("instrument_id", "integer", (col) =>
      col.notNull().references("instruments.id"),
    )
    .addColumn("from_ts", "timestamptz", (col) => col.notNull())
    .addColumn("to_ts", "timestamptz", (col) => col.notNull())
    .addColumn("status", "text", (col) => col.notNull().defaultTo("queued"))
    .addColumn("checkpoint_ts", "timestamptz")
    .addColumn("error", "text")
    .addColumn("created_at", "timestamptz", (col) => col.notNull().defaultTo(sql`now()`))
    .addColumn("updated_at", "timestamptz", (col) => col.notNull().defaultTo(sql`now()`))
    .execute();

  await sql`ALTER TABLE backfill_jobs ADD COLUMN requested_by text[] NOT NULL DEFAULT '{}'`.execute(
    db,
  );
  await sql`CREATE INDEX backfill_jobs_instrument_status_idx ON backfill_jobs (instrument_id, status)`.execute(
    db,
  );

  await db.schema
    .createTable("collection_targets")
    .addColumn("instrument_id", "integer", (col) =>
      col.primaryKey().references("instruments.id"),
    )
    .addColumn("enabled", "boolean", (col) => col.notNull().defaultTo(sql`false`))
    .addColumn("last_collected_ts", "timestamptz")
    .addColumn("updated_at", "timestamptz", (col) => col.notNull().defaultTo(sql`now()`))
    .execute();
}

export async function down(db: Kysely<any>): Promise<void> {
  await db.schema.dropTable("collection_targets").ifExists().execute();
  await db.schema.dropTable("backfill_jobs").ifExists().execute();
  await db.schema.dropTable("service_state").ifExists().execute();
}
