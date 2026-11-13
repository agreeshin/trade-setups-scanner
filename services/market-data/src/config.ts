import { DEFAULT_FRESH_OPTIONS, type FreshOptions } from "./ingest/fresh.js";
import { DEFAULT_GATE_LIMITS, type GateLimits } from "./ingest/gate.js";

export interface AppConfig {
  bybit: { requestTimeoutMs: number; gate: GateLimits; fresh: FreshOptions };
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
  const bounded = (key: string, fallback: number, max: number) => {
    const value = positiveInteger(env, key, fallback);
    if (value > max) throw new Error(`${key} must be <= ${max}`);
    return value;
  };
  const rps = bounded("BYBIT_REST_MAX_RPS", 30, 60);
  return {
    bybit: {
      requestTimeoutMs: bounded("BYBIT_REQUEST_TIMEOUT_MS", 5000, 60_000),
      gate: { ...DEFAULT_GATE_LIMITS, maxPerWindow: rps * 5, minIntervalMs: Math.ceil(1000 / rps), initialIntervalMs: Math.max(100, Math.ceil(1000 / rps)) },
      fresh: {
        ...DEFAULT_FRESH_OPTIONS,
        tickMs: bounded("BYBIT_FRESH_TICK_MS", 2000, 10_000),
        fallbackDelayMs: bounded("BYBIT_FRESH_FALLBACK_DELAY_MS", 3000, 60_000),
        repairMinutes: bounded("BYBIT_FRESH_REPAIR_MINUTES", 5, 30),
        maxSymbols: bounded("BYBIT_FRESH_MAX_SYMBOLS", 4096, 10_000),
        concurrency: bounded("BYBIT_FRESH_CONCURRENCY", 8, 16),
        ws: { ...DEFAULT_FRESH_OPTIONS.ws,
          shardSymbols: bounded("BYBIT_WS_SHARD_SYMBOLS", 200, 200),
          maxPayloadBytes: bounded("BYBIT_WS_MAX_PAYLOAD_BYTES", 256 * 1024, 1024 * 1024),
        },
      },
    },
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
