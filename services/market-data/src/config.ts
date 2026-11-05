export interface AppConfig {
  databaseUrl: string;
  testDatabaseUrl?: string | undefined;
  apiKeys: string[];
  port: number;
  host: string;
  logLevel: string;
  backupDir: string;
  backupMaxUploadBytes: number;
  backfillLimits: {
    maxPendingJobs: number;
    maxWorkMinutes: number;
    maxDatabaseBytes: number;
    estimatedBytesPerWorkMinute: number;
  };
}

function required(env: Record<string, string | undefined>, key: string): string {
  const value = env[key];
  if (value === undefined || value.trim() === "") {
    throw new Error(`Missing required environment variable: ${key}`);
  }
  return value;
}

function positiveInteger(env: Record<string, string | undefined>, key: string, fallback: number): number {
  const value = Number(env[key] ?? fallback);
  if (!Number.isSafeInteger(value) || value < 1) throw new Error(`${key} must be a positive safe integer`);
  return value;
}

export function loadConfig(env: Record<string, string | undefined> = process.env): AppConfig {
  return {
    databaseUrl: required(env, "DATABASE_URL"),
    testDatabaseUrl: env.TEST_DATABASE_URL,
    apiKeys: (env.API_KEYS ?? "")
      .split(",")
      .map((key) => key.trim())
      .filter((key) => key.length > 0),
    port: Number(env.PORT ?? 8081),
    host: env.HOST ?? "127.0.0.1",
    logLevel: env.LOG_LEVEL ?? "info",
    backupDir: env.BACKUP_DIR ?? "./backups",
    backupMaxUploadBytes: positiveInteger(env, "BACKUP_MAX_UPLOAD_BYTES", 120 * 1024 ** 3),
    backfillLimits: {
      maxPendingJobs: positiveInteger(env, "BACKFILL_MAX_PENDING_JOBS", 100),
      maxWorkMinutes: positiveInteger(env, "BACKFILL_MAX_WORK_MINUTES", 2_635_200),
      maxDatabaseBytes: positiveInteger(env, "MARKET_DATA_MAX_DATABASE_BYTES", 100 * 1024 ** 3),
      estimatedBytesPerWorkMinute: positiveInteger(env, "BACKFILL_ESTIMATED_BYTES_PER_MINUTE", 512),
    },
  };
}
