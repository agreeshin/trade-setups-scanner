/* eslint-disable @typescript-eslint/no-explicit-any -- official Kysely migration
   signature: migrations create the schema, so they use a permissive db view. */
import { sql, type Kysely } from "kysely";

export async function up(db: Kysely<any>): Promise<void> {
  await db.schema
    .createTable("gap_verifications")
    .addColumn("id", "integer", (col) => col.generatedAlwaysAsIdentity().primaryKey())
    .addColumn("instrument_id", "integer", (col) => col.notNull())
    .addColumn("from_ts", "timestamptz", (col) => col.notNull())
    .addColumn("to_ts", "timestamptz", (col) => col.notNull())
    .addColumn("kind", "text", (col) => col.notNull().defaultTo("no_trade"))
    .addColumn("source", "text", (col) => col.notNull())
    .addColumn("verified_at", "timestamptz", (col) => col.notNull().defaultTo(sql`now()`))
    .execute();
  await sql`CREATE INDEX gap_verifications_instrument_idx ON gap_verifications (instrument_id, from_ts, to_ts)`.execute(
    db,
  );

  await db.schema
    .createTable("gap_repair_jobs")
    .addColumn("id", "integer", (col) => col.generatedAlwaysAsIdentity().primaryKey())
    .addColumn("instrument_id", "integer", (col) => col.notNull())
    .addColumn("status", "text", (col) => col.notNull().defaultTo("queued"))
    .addColumn("gaps_processed", "integer", (col) => col.notNull().defaultTo(0))
    .addColumn("filled_minutes", "integer", (col) => col.notNull().defaultTo(0))
    .addColumn("verified_minutes", "integer", (col) => col.notNull().defaultTo(0))
    .addColumn("error", "text")
    .addColumn("created_at", "timestamptz", (col) => col.notNull().defaultTo(sql`now()`))
    .addColumn("updated_at", "timestamptz", (col) => col.notNull().defaultTo(sql`now()`))
    .execute();
}

export async function down(db: Kysely<any>): Promise<void> {
  await db.schema.dropTable("gap_repair_jobs").ifExists().execute();
  await db.schema.dropTable("gap_verifications").ifExists().execute();
}
