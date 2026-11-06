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

export interface EnsuredInstrument {
  id: number;
  symbol: string;
  status: string;
}

export async function ensureInstruments(
  db: Kysely<MarketDataDB>,
  identity: { exchangeCode: string; category: string },
  symbols: string[],
): Promise<EnsuredInstrument[]> {
  if (symbols.length === 0) return [];
  const exchangeId = await ensureExchange(db, identity.exchangeCode, identity.exchangeCode);
  const existing = await db
    .selectFrom("instruments")
    .select(["id", "symbol", "status"])
    .where((eb) => eb.and([
      eb("exchange_id", "=", exchangeId),
      eb("category", "=", identity.category),
      eb("symbol", "in", symbols),
    ]))
    .execute();
  const known = new Set(existing.map((row) => row.symbol));
  const missing = [...new Set(symbols)].filter((symbol) => !known.has(symbol));
  const inserted = missing.length === 0 ? [] : await db
    .insertInto("instruments")
    .values(missing.map((symbol) => ({
      exchange_id: exchangeId,
      category: identity.category,
      symbol,
      status: "active",
      listed_at: null,
      delisted_at: null,
    })))
    .onConflict((oc) => oc.constraint("instruments_identity_unique").doUpdateSet({ category: identity.category }))
    .returning(["id", "symbol"])
    .execute();
  return [
    ...existing.map((row) => ({ id: row.id, symbol: row.symbol, status: row.status })),
    ...inserted.map((row) => ({ id: row.id, symbol: row.symbol, status: "active" })),
  ].sort((a, b) => a.symbol.localeCompare(b.symbol));
}
