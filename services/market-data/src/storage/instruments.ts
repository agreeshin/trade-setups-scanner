import type { Kysely } from "kysely";
import type { MarketDataDB } from "../db/schema.js";

export interface InstrumentIdentity {
  exchangeCode: string;
  category: string;
  symbol: string;
}

export async function ensureExchange(
  db: Kysely<MarketDataDB>,
  code: string,
  name: string,
): Promise<number> {
  const row = await db
    .insertInto("exchanges")
    .values({ code, name })
    .onConflict((oc) => oc.column("code").doUpdateSet({ name }))
    .returning("id")
    .executeTakeFirstOrThrow();
  return row.id;
}

export async function ensureInstrument(
  db: Kysely<MarketDataDB>,
  identity: InstrumentIdentity,
): Promise<number> {
  const exchangeId = await ensureExchange(db, identity.exchangeCode, identity.exchangeCode);
  const inserted = await db
    .insertInto("instruments")
    .values({
      exchange_id: exchangeId,
      category: identity.category,
      symbol: identity.symbol,
      status: "active",
      listed_at: null,
      delisted_at: null,
    })
    .onConflict((oc) =>
      oc
        .constraint("instruments_identity_unique")
        .doUpdateSet({ category: identity.category }),
    )
    .returning("id")
    .executeTakeFirstOrThrow();
  return inserted.id;
}
