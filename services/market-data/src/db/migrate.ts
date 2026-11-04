import "dotenv/config";
 
import { createDb } from "./db.js";
import { createMigrator } from "./migrator.js";
import { loadConfig } from "../config.js";

const config = loadConfig();
const useTestDatabase = process.argv.includes("--test");
const databaseUrl = useTestDatabase ? config.testDatabaseUrl : config.databaseUrl;
if (!databaseUrl) {
  console.error("TEST_DATABASE_URL is required when --test is used");
  process.exit(1);
}

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
