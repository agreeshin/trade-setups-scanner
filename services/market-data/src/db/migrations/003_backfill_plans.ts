/* eslint-disable @typescript-eslint/no-explicit-any -- official Kysely migration
   signature: migrations create the schema, so they use a permissive db view. */
import { sql, type Kysely } from "kysely";

export async function up(db: Kysely<any>): Promise<void> {
  await db.schema
    .createTable("backfill_plans")
    .addColumn("id", "integer", (col) => col.generatedAlwaysAsIdentity().primaryKey())
    .addColumn("from_ts", "timestamptz", (col) => col.notNull())
    .addColumn("to_ts", "timestamptz", (col) => col.notNull())
    .addColumn("status", "text", (col) => col.notNull().defaultTo("active"))
    .addColumn("requested_by", "text", (col) => col.notNull())
    .addColumn("created_at", "timestamptz", (col) => col.notNull().defaultTo(sql`now()`))
    .addColumn("updated_at", "timestamptz", (col) => col.notNull().defaultTo(sql`now()`))
    .execute();

  await db.schema
    .createTable("backfill_plan_items")
    .addColumn("plan_id", "integer", (col) =>
      col.notNull().references("backfill_plans.id"),
    )
    .addColumn("instrument_id", "integer", (col) => col.notNull())
    .addColumn("status", "text", (col) => col.notNull().defaultTo("pending"))
    .addColumn("job_id", "integer")
    .addColumn("error", "text")
    .addColumn("updated_at", "timestamptz", (col) => col.notNull().defaultTo(sql`now()`))
    .addPrimaryKeyConstraint("backfill_plan_items_pk", ["plan_id", "instrument_id"])
    .execute();

  await sql`CREATE INDEX backfill_plan_items_plan_status_idx ON backfill_plan_items (plan_id, status)`.execute(
    db,
  );
}

export async function down(db: Kysely<any>): Promise<void> {
  await db.schema.dropTable("backfill_plan_items").ifExists().execute();
  await db.schema.dropTable("backfill_plans").ifExists().execute();
}
