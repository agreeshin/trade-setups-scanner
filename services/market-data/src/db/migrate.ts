import "dotenv/config";
/* eslint-disable @typescript-eslint/no-unsafe-assignment, @typescript-eslint/no-unsafe-call, @typescript-eslint/no-unsafe-member-access --
   kysely/migration types resolve in tsc but not in the ESLint project service. */
import { createDb } from "./db.js";
import { createMigrator } from "./migrator.js";
import { loadConfig } from "../config.js";

const config = loadConfig();
const useTestDatabase = process.argv.includes("--test");
const databaseUrl = useTestDatabase ? config.testDatabaseUrl : config.databaseUrl;

const db = createDb(databaseUrl);
const migrator = createMigrator(db);

const result = await migrator.migrateToLatest();
db.destroy().catch(() => undefined);

if (result.error) {
  console.error("Migration failed:", result.error);
  process.exit(1);
}

console.log(
  `Migrations applied to ${useTestDatabase ? "test" : "main"} database:`,
  result.results?.map((migration) => `${migration.status}:${migration.migrationName}`).join(", "),
);
