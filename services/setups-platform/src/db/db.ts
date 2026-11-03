import { Kysely, PostgresDialect } from "kysely";
import { Pool } from "pg";
import type { SignalsDB } from "./schema.js";

export function createDb(databaseUrl: string): Kysely<SignalsDB> {
  return new Kysely<SignalsDB>({
    dialect: new PostgresDialect({
      pool: new Pool({ connectionString: databaseUrl, max: 10 }),
    }),
  });
}
