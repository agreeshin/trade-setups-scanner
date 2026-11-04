import { sql, type Kysely } from "kysely";

export async function up<DB>(db: Kysely<DB>): Promise<void> {
  await db.schema.createTable("push_subscriptions")
    .addColumn("endpoint", "text", (col) => col.primaryKey())
    .addColumn("p256dh", "text", (col) => col.notNull())
    .addColumn("auth", "text", (col) => col.notNull())
    .addColumn("created_at", "timestamptz", (col) => col.notNull().defaultTo(sql`now()`))
    .execute();
}

export async function down<DB>(db: Kysely<DB>): Promise<void> {
  await db.schema.dropTable("push_subscriptions").execute();
}
