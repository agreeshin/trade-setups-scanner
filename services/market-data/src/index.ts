import "dotenv/config";
import { buildApp } from "./app.js";
import { createDb } from "./db/db.js";
import { createMigrator } from "./db/migrator.js";
import { loadConfig } from "./config.js";
import { startIngestRuntime, type IngestRuntime } from "./ingest/runtime.js";
import { BybitClient } from "./ingest/bybit.js";
import { AdaptiveGate, DEFAULT_GATE_LIMITS } from "./ingest/gate.js";
import { RealClock } from "./ingest/clock.js";
import { FetchTransport } from "./ingest/transport.js";
import { BackupManager } from "./backup/routes.js";

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
  backfillLimits: config.backfillLimits,
  backupManager: new BackupManager({
    scope: "market-data",
    databaseUrl: config.databaseUrl,
    backupDir: config.backupDir,
    maxUploadBytes: config.backupMaxUploadBytes,
  }),
  exchangeListing: () => new BybitClient(
    new FetchTransport(),
    new AdaptiveGate(DEFAULT_GATE_LIMITS, new RealClock()),
    new RealClock(),
  ).fetchLinearPerpetualUsdtSymbols(),
});
let ingestRuntime: IngestRuntime | undefined;

if (process.env.BYBIT_INGEST_ENABLED === "1") {
  ingestRuntime = await startIngestRuntime({ db, databaseUrl: config.databaseUrl, backfillLimits: config.backfillLimits });
  app.log.info("bybit ingest runtime started");
}

async function shutdown(): Promise<void> {
  if (ingestRuntime) {
    await ingestRuntime.stop();
  }
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
