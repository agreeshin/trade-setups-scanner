import "dotenv/config";
import { buildApp } from "./app.js";
import { createDb } from "./db/db.js";
import { createMigrator } from "./db/migrator.js";
import { loadConfig } from "./config.js";
import { startLiveRuntime } from "./live/runtime.js";
import { setSetting } from "./settings.js";
import { startReplayJobs } from "./replay/jobs.js";
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
  marketData: { url: config.marketDataUrl, apiKey: config.marketDataApiKey },
  replayLimits: config.replayLimits,
  backupManager: new BackupManager({
    scope: "platform",
    databaseUrl: config.databaseUrl,
    backupDir: config.backupDir,
    maxUploadBytes: config.backupMaxUploadBytes,
  }),
  marketBackupMaxUploadBytes: config.marketBackupMaxUploadBytes,
});
await setSetting(db, "live-status", { running: false, lastScanAt: null, lastError: null });
startLiveRuntime(app, db, { url: config.marketDataUrl, apiKey: config.marketDataApiKey });
const replayJobs = await startReplayJobs(db, { url: config.marketDataUrl, apiKey: config.marketDataApiKey }, {
  onError: () => app.log.error("replay job failed; inspect job status"),
  admissionLimits: config.replayLimits,
});
app.addHook("onClose", () => replayJobs.stop());

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
