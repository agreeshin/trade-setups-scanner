import { Kysely, PostgresDialect } from "kysely";
import { Pool } from "pg";
import type { MarketDataDB } from "./schema.js";

export function createDb(databaseUrl: string): Kysely<MarketDataDB> {
  return new Kysely<MarketDataDB>({
    dialect: new PostgresDialect({
      pool: new Pool({ connectionString: databaseUrl, max: 10 }),
    }),
  });
}
