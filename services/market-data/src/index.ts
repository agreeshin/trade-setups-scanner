import "dotenv/config";
import { buildApp } from "./app.js";
import { createDb } from "./db/db.js";
import { loadConfig } from "./config.js";

const config = loadConfig();
const db = createDb(config.databaseUrl);
const app = await buildApp({ db, apiKeys: config.apiKeys });

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
    app.log.info(`market-data service listening at ${address}`);
  })
  .catch((error) => {
    app.log.error(error, "failed to start market-data service");
    process.exit(1);
  });
