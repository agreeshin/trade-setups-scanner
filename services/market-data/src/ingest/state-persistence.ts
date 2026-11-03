import type { Kysely } from "kysely";
import type { MarketDataDB } from "../db/schema.js";

const STATE_KEY = "bybit_gate";

export function createGatePersistence(db: Kysely<MarketDataDB>) {
  return {
    async loadCooldown(): Promise<number | null> {
      const row = await db
        .selectFrom("service_state")
        .where("key", "=", STATE_KEY)
        .select("value")
        .executeTakeFirst();
      const iso = row?.value.cooldownUntilIso ?? null;
      return iso === null ? null : new Date(iso).getTime();
    },

    async saveCooldown(untilMs: number): Promise<void> {
      const value = { cooldownUntilIso: new Date(untilMs).toISOString() };
      await db
        .insertInto("service_state")
        .values({ key: STATE_KEY, value })
        .onConflict((oc) =>
          oc.column("key").doUpdateSet({ value, updated_at: new Date() }),
        )
        .execute();
    },
  };
}
