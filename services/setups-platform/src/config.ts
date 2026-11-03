export interface PlatformConfig {
  databaseUrl: string;
  apiKeys: string[];
  marketDataUrl: string;
  marketDataApiKey: string;
  port: number;
  host: string;
  logLevel: string;
}

function required(env: Record<string, string | undefined>, key: string): string {
  const value = env[key];
  if (value === undefined || value.trim() === "") {
    throw new Error(`Missing required environment variable: ${key}`);
  }
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
  };
}
