import type { Kysely } from "kysely";

export async function up(db: Kysely<unknown>): Promise<void> {
  await db.schema.alterTable("live_setups").addColumn("details", "jsonb").execute();
}

export async function down(db: Kysely<unknown>): Promise<void> {
  await db.schema.alterTable("live_setups").dropColumn("details").execute();
}
