import "dotenv/config";
import { buildApp } from "./app.js";
import { createDb } from "./db/db.js";
import { createMigrator } from "./db/migrator.js";
import { loadConfig } from "./config.js";

const config = loadConfig();
const db = createDb(config.databaseUrl);

const migrator = createMigrator(db);
const migration = await migrator.migrateToLatest();
if (migration.error) {
  console.error("startup migration failed:", migration.error);
  process.exit(1);
}

const app = await buildApp({
  db,
  apiKeys: config.apiKeys,
  marketData: { url: config.marketDataUrl, apiKey: config.marketDataApiKey },
});

async function shutdown(): Promise<void> {
  await app.close();
  await db.destroy();
}

process.once("SIGINT", () => {
  void shutdown().finally(() => process.exit(0));
});
process.once("SIGTERM", () => {
  void shutdown().finally(() => process.exit(0));
});

app
  .listen({ port: config.port, host: config.host })
  .then((address) => {
    app.log.info(`signals platform listening at ${address}`);
  })
  .catch((error) => {
    app.log.error(error, "failed to start signals platform");
    process.exit(1);
  });
