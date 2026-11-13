/* eslint-disable @typescript-eslint/no-explicit-any -- Kysely migration schema view. */
import { sql, type Kysely } from "kysely";

export async function up(db: Kysely<any>): Promise<void> {
  await db.schema.createTable("instrument_watermarks")
    .addColumn("instrument_id", "integer", (c) => c.primaryKey().references("instruments.id").onDelete("cascade"))
    .addColumn("latest_open_time", "timestamptz")
    .addColumn("latest_received_at", "timestamptz")
    .execute();
  await db.schema.alterTable("collection_targets").addColumn("tail_next_ts", "timestamptz").execute();
  await db.schema.alterTable("collection_targets").addColumn("tail_initialized", "boolean", (c) => c.notNull().defaultTo(false)).execute();
  await sql`ALTER TABLE instrument_watermarks ADD CONSTRAINT watermark_aligned CHECK
    (latest_open_time IS NULL OR mod(extract(epoch FROM latest_open_time), 60) = 0)`.execute(db);
}

export async function down(db: Kysely<any>): Promise<void> {
  await db.schema.alterTable("collection_targets").dropColumn("tail_initialized").execute();
  await db.schema.alterTable("collection_targets").dropColumn("tail_next_ts").execute();
  await db.schema.dropTable("instrument_watermarks").execute();
}
