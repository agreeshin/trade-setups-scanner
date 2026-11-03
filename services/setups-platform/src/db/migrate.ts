import "dotenv/config";
 
import { createDb } from "./db.js";
import { createMigrator } from "./migrator.js";

const databaseUrl = process.argv.includes("--test")
  ? process.env.TEST_DATABASE_URL
  : process.env.DATABASE_URL;

if (!databaseUrl) {
  console.error("DATABASE_URL (or TEST_DATABASE_URL with --test) is required");
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
  "Migrations applied:",
  result.results?.map((migration) => `${migration.status}:${migration.migrationName}`).join(", "),
);
