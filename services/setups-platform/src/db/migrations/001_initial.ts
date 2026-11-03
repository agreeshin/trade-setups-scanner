/* eslint-disable @typescript-eslint/no-explicit-any -- official Kysely migration
   signature: migrations create the schema, so they use a permissive db view. */
import { sql, type Kysely } from "kysely";

export async function up(db: Kysely<any>): Promise<void> {
  await db.schema
    .createTable("replay_runs")
    .addColumn("id", "integer", (col) => col.generatedAlwaysAsIdentity().primaryKey())
    .addColumn("exchange_code", "text", (col) => col.notNull())
    .addColumn("category", "text", (col) => col.notNull())
    .addColumn("symbol", "text", (col) => col.notNull())
    .addColumn("from_ts", "timestamptz", (col) => col.notNull())
    .addColumn("to_ts", "timestamptz", (col) => col.notNull())
    .addColumn("algorithm_id", "text", (col) => col.notNull())
    .addColumn("algorithm_version", "text", (col) => col.notNull())
    .addColumn("params", "jsonb", (col) => col.notNull())
    .addColumn("status", "text", (col) => col.notNull().defaultTo("queued"))
    .addColumn("cache_hit", "boolean", (col) => col.notNull().defaultTo(sql`false`))
    .addColumn("setups", "jsonb")
    .addColumn("warmup_minutes", "integer")
    .addColumn("processed_minutes", "integer")
    .addColumn("error", "text")
    .addColumn("created_at", "timestamptz", (col) => col.notNull().defaultTo(sql`now()`))
    .addColumn("updated_at", "timestamptz", (col) => col.notNull().defaultTo(sql`now()`))
    .execute();

  await db.schema
    .createTable("replay_cache")
    .addColumn("cache_key", "text", (col) => col.primaryKey())
    .addColumn("exchange_code", "text", (col) => col.notNull())
    .addColumn("category", "text", (col) => col.notNull())
    .addColumn("symbol", "text", (col) => col.notNull())
    .addColumn("algorithm_id", "text", (col) => col.notNull())
    .addColumn("algorithm_version", "text", (col) => col.notNull())
    .addColumn("params", "jsonb", (col) => col.notNull())
    .addColumn("from_ts", "timestamptz", (col) => col.notNull())
    .addColumn("to_ts", "timestamptz", (col) => col.notNull())
    .addColumn("setups", "jsonb", (col) => col.notNull())
    .addColumn("warmup_minutes", "integer", (col) => col.notNull())
    .addColumn("processed_minutes", "integer", (col) => col.notNull())
    .addColumn("computed_at", "timestamptz", (col) => col.notNull().defaultTo(sql`now()`))
    .execute();

  await db.schema
    .createTable("settings")
    .addColumn("key", "text", (col) => col.primaryKey())
    .addColumn("value", "jsonb", (col) => col.notNull())
    .addColumn("updated_at", "timestamptz", (col) => col.notNull().defaultTo(sql`now()`))
    .execute();

  await db.schema
    .createTable("live_setups")
    .addColumn("id", "integer", (col) => col.generatedAlwaysAsIdentity().primaryKey())
    .addColumn("exchange_code", "text", (col) => col.notNull())
    .addColumn("category", "text", (col) => col.notNull())
    .addColumn("symbol", "text", (col) => col.notNull())
    .addColumn("algorithm_id", "text", (col) => col.notNull())
    .addColumn("algorithm_version", "text", (col) => col.notNull())
    .addColumn("params", "jsonb", (col) => col.notNull())
    .addColumn("open_time", "timestamptz", (col) => col.notNull())
    .addColumn("close_time", "timestamptz", (col) => col.notNull())
    .addColumn("close_price", "text", (col) => col.notNull())
    .addColumn("notified_at", "timestamptz")
    .addColumn("created_at", "timestamptz", (col) => col.notNull().defaultTo(sql`now()`))
    .addUniqueConstraint("live_setups_identity_unique", [
      "exchange_code",
      "category",
      "symbol",
      "algorithm_id",
      "algorithm_version",
      "open_time",
    ])
    .execute();

  await db.schema
    .createTable("instrument_lists")
    .addColumn("id", "integer", (col) => col.generatedAlwaysAsIdentity().primaryKey())
    .addColumn("list_type", "text", (col) => col.notNull())
    .addColumn("exchange_code", "text", (col) => col.notNull())
    .addColumn("category", "text", (col) => col.notNull())
    .addColumn("symbol", "text", (col) => col.notNull())
    .addColumn("created_at", "timestamptz", (col) => col.notNull().defaultTo(sql`now()`))
    .addUniqueConstraint("instrument_lists_identity_unique", [
      "list_type",
      "exchange_code",
      "category",
      "symbol",
    ])
    .execute();
}

export async function down(db: Kysely<any>): Promise<void> {
  await db.schema.dropTable("instrument_lists").ifExists().execute();
  await db.schema.dropTable("live_setups").ifExists().execute();
  await db.schema.dropTable("settings").ifExists().execute();
  await db.schema.dropTable("replay_cache").ifExists().execute();
  await db.schema.dropTable("replay_runs").ifExists().execute();
}
