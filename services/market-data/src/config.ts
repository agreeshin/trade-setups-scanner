export interface AppConfig {
  databaseUrl: string;
  testDatabaseUrl: string;
  apiKeys: string[];
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

export function loadConfig(env: Record<string, string | undefined> = process.env): AppConfig {
  return {
    databaseUrl: required(env, "DATABASE_URL"),
    testDatabaseUrl: required(env, "TEST_DATABASE_URL"),
    apiKeys: (env.API_KEYS ?? "")
      .split(",")
      .map((key) => key.trim())
      .filter((key) => key.length > 0),
    port: Number(env.PORT ?? 8081),
    host: env.HOST ?? "127.0.0.1",
    logLevel: env.LOG_LEVEL ?? "info",
  };
}
