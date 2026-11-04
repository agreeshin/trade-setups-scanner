import { FileMigrationProvider, Migrator } from "kysely/migration";
import { type Kysely } from "kysely";
import fs from "node:fs/promises";
import path from "node:path";
import { fileURLToPath } from "node:url";
import type { MarketDataDB } from "./schema.js";

export function createMigrator(db: Kysely<MarketDataDB>): Migrator {
  return new Migrator({
    db,
    provider: new FileMigrationProvider({
      fs,
      path,
      migrationFolder: path.join(
        path.dirname(fileURLToPath(import.meta.url)),
        "migrations",
      ),
    }),
  });
}
