/* eslint-disable @typescript-eslint/no-explicit-any -- official Kysely migration
   signature: migrations create the schema, so they use a permissive db view. */
import { sql, type Kysely } from "kysely";

export async function up(db: Kysely<any>): Promise<void> {
  await db.schema
    .createTable("exchanges")
    .addColumn("id", "integer", (col) => col.generatedAlwaysAsIdentity().primaryKey())
    .addColumn("code", "text", (col) => col.notNull().unique())
    .addColumn("name", "text", (col) => col.notNull())
    .addColumn("created_at", "timestamptz", (col) => col.notNull().defaultTo(sql`now()`))
    .execute();

  await db.schema
    .createTable("instruments")
    .addColumn("id", "integer", (col) => col.generatedAlwaysAsIdentity().primaryKey())
    .addColumn("exchange_id", "integer", (col) =>
      col.notNull().references("exchanges.id"),
    )
    .addColumn("category", "text", (col) => col.notNull())
    .addColumn("symbol", "text", (col) => col.notNull())
    .addColumn("status", "text", (col) => col.notNull().defaultTo("active"))
    .addColumn("listed_at", "timestamptz")
    .addColumn("delisted_at", "timestamptz")
    .addColumn("created_at", "timestamptz", (col) => col.notNull().defaultTo(sql`now()`))
    .addUniqueConstraint("instruments_identity_unique", [
      "exchange_id",
      "category",
      "symbol",
    ])
    .execute();

  await sql`CREATE TABLE candles (
    instrument_id integer NOT NULL REFERENCES instruments(id),
    open_time timestamptz NOT NULL,
    open numeric NOT NULL,
    high numeric NOT NULL,
    low numeric NOT NULL,
    close numeric NOT NULL,
    volume numeric NOT NULL,
    quote_turnover numeric NOT NULL,
    source text NOT NULL DEFAULT 'bybit',
    ingested_at timestamptz NOT NULL DEFAULT now(),
    PRIMARY KEY (instrument_id, open_time)
  ) PARTITION BY RANGE (open_time)`.execute(db);

  await db.schema
    .createTable("candle_conflicts")
    .addColumn("id", "integer", (col) => col.generatedAlwaysAsIdentity().primaryKey())
    .addColumn("instrument_id", "integer", (col) =>
      col.notNull().references("instruments.id"),
    )
    .addColumn("open_time", "timestamptz", (col) => col.notNull())
    .addColumn("existing_record", "jsonb", (col) => col.notNull())
    .addColumn("incoming_record", "jsonb", (col) => col.notNull())
    .addColumn("detected_at", "timestamptz", (col) => col.notNull().defaultTo(sql`now()`))
    .addUniqueConstraint("candle_conflicts_unique", ["instrument_id", "open_time"])
    .execute();
}

export async function down(db: Kysely<any>): Promise<void> {
  await db.schema.dropTable("candle_conflicts").ifExists().execute();
  await db.schema.dropTable("candles").ifExists().execute();
  await db.schema.dropTable("instruments").ifExists().execute();
  await db.schema.dropTable("exchanges").ifExists().execute();
}
