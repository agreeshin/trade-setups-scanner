/* eslint-disable @typescript-eslint/no-explicit-any -- Kysely migration schema view. */
import { randomUUID } from "node:crypto";
import { sql, type Kysely } from "kysely";

export async function up(db: Kysely<any>): Promise<void> {
  await sql`CREATE TABLE candle_source_identity (
    id integer PRIMARY KEY CHECK (id = 1), epoch uuid NOT NULL
  )`.execute(db);
  await sql`INSERT INTO candle_source_identity (id, epoch) VALUES (1, ${randomUUID()}::uuid)`.execute(db);
  await sql`CREATE TABLE candle_insert_heads (
    instrument_id integer PRIMARY KEY REFERENCES instruments(id) ON DELETE CASCADE,
    revision bigint NOT NULL DEFAULT 0,
    pruned_through_revision bigint NOT NULL DEFAULT 0,
    ready boolean NOT NULL DEFAULT true,
    CHECK (pruned_through_revision >= 0 AND revision >= pruned_through_revision)
  )`.execute(db);
  // Existing immutable candles are revision zero. Only the small instrument
  // catalogue is read; no candle/history bootstrap is needed.
  await sql`INSERT INTO candle_insert_heads (instrument_id) SELECT id FROM instruments`.execute(db);
  await sql`CREATE TABLE candle_insert_journal (
    instrument_id integer NOT NULL REFERENCES candle_insert_heads(instrument_id) ON DELETE CASCADE,
    revision bigint NOT NULL CHECK (revision > 0),
    from_ts timestamptz NOT NULL,
    to_ts timestamptz NOT NULL,
    PRIMARY KEY (instrument_id, revision), CHECK (from_ts < to_ts)
  )`.execute(db);
  // New instruments have a known empty baseline, including bulk catalogue
  // creation. Missing metadata on an existing instrument is never auto-blessed.
  await sql`CREATE FUNCTION initialize_candle_insert_head() RETURNS trigger LANGUAGE plpgsql AS $$
    BEGIN
      INSERT INTO candle_insert_heads (instrument_id) VALUES (NEW.id);
      RETURN NEW;
    END
  $$`.execute(db);
  await sql`CREATE TRIGGER instrument_candle_insert_head AFTER INSERT ON instruments
    FOR EACH ROW EXECUTE FUNCTION initialize_candle_insert_head()`.execute(db);
}

export async function down(db: Kysely<any>): Promise<void> {
  await sql`DROP TRIGGER instrument_candle_insert_head ON instruments`.execute(db);
  await sql`DROP FUNCTION initialize_candle_insert_head()`.execute(db);
  await db.schema.dropTable("candle_insert_journal").execute();
  await db.schema.dropTable("candle_insert_heads").execute();
  await db.schema.dropTable("candle_source_identity").execute();
}
