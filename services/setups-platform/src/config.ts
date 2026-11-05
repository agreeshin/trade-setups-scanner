export interface PlatformConfig {
  databaseUrl: string;
  apiKeys: string[];
  marketDataUrl: string;
  marketDataApiKey: string;
  port: number;
  host: string;
  logLevel: string;
  backupDir: string;
  backupMaxUploadBytes: number;
  marketBackupMaxUploadBytes: number;
  replayLimits: {
    maxPendingRuns: number;
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

export function loadConfig(
  env: Record<string, string | undefined> = process.env,
): PlatformConfig {
  return {
    databaseUrl: required(env, "DATABASE_URL"),
    apiKeys: (env.API_KEYS ?? "")
      .split(",")
      .map((key) => key.trim())
      .filter((key) => key.length > 0),
    marketDataUrl: required(env, "MARKET_DATA_URL"),
    marketDataApiKey: required(env, "MARKET_DATA_API_KEY"),
    port: Number(env.PORT ?? 8082),
    host: env.HOST ?? "127.0.0.1",
    logLevel: env.LOG_LEVEL ?? "info",
    backupDir: env.BACKUP_DIR ?? "./backups",
    backupMaxUploadBytes: positiveInteger(env, "BACKUP_MAX_UPLOAD_BYTES", 60 * 1024 ** 3),
    marketBackupMaxUploadBytes: positiveInteger(env, "MARKET_DATA_BACKUP_MAX_UPLOAD_BYTES", 120 * 1024 ** 3),
    replayLimits: {
      maxPendingRuns: positiveInteger(env, "REPLAY_MAX_PENDING_RUNS", 100),
      maxWorkMinutes: positiveInteger(env, "REPLAY_MAX_WORK_MINUTES", 52_704_000),
      maxDatabaseBytes: positiveInteger(env, "REPLAY_MAX_DATABASE_BYTES", 50 * 1024 ** 3),
      estimatedBytesPerWorkMinute: positiveInteger(env, "REPLAY_ESTIMATED_BYTES_PER_MINUTE", 128),
    },
  };
}
