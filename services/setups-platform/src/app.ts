import Fastify, { type FastifyInstance } from "fastify";
import swagger from "@fastify/swagger";
import { Type, type Static } from "@sinclair/typebox";
import { sql, type Kysely } from "kysely";
import type { SignalsDB } from "./db/schema.js";
import { createApiKeyGuard } from "./auth.js";
import { registerPlatformV1Routes } from "./routes/v1.js";

export interface ReadinessCheck {
  name: string;
  check: () => Promise<void>;
}
export interface AppOptions {
  readinessChecks?: ReadinessCheck[];
  openApiEnabled?: boolean;
  db?: Kysely<SignalsDB>;
  apiKeys?: string[];
  marketData?: { url: string; apiKey: string };
}
const HealthResponse = Type.Object({
  status: Type.Literal("ok"),
  service: Type.String(),
  time: Type.String({ format: "date-time" }),
});
const ReadinessResponse = Type.Object({
  status: Type.Union([Type.Literal("ready"), Type.Literal("not_ready")]),
  service: Type.String(),
  checks: Type.Array(
    Type.Object({
      name: Type.String(),
      ok: Type.Boolean(),
      detail: Type.Optional(Type.String()),
    }),
  ),
});
export type HealthResponseBody = Static<typeof HealthResponse>;
export type ReadinessResponseBody = Static<typeof ReadinessResponse>;
export async function buildApp(options: AppOptions = {}): Promise<FastifyInstance> {
  const readinessChecks: ReadinessCheck[] = [...(options.readinessChecks ?? [])];
  if (options.db) {
    const db = options.db;
    readinessChecks.push({
      name: "database",
      check: async () => {
        await sql`select 1`.execute(db);
      },
    });
  }
  const app = Fastify({
    logger: {
      level: process.env.LOG_LEVEL ?? "info",
    },
  });
  if (options.openApiEnabled !== false) {
    await app.register(swagger, {
      openapi: {
        info: {
          title: "Signals Platform API",
          description:
            "Setup detection platform: detector runs live and replay, owns results, cache, and notifications.",
          version: "0.1.0",
        },
      },
    });
  }
  app.get(
    "/health",
    {
      schema: {
        response: { 200: HealthResponse },
      },
    },
    (): HealthResponseBody => ({
      status: "ok",
      service: "signals-platform",
      time: new Date().toISOString(),
    }),
  );
  app.get(
    "/ready",
    {
      schema: {
        response: { 200: ReadinessResponse, 503: ReadinessResponse },
      },
    },
    async (_request, reply) => {
      const results: ReadinessResponseBody["checks"] = [];
      for (const check of readinessChecks) {
        try {
          await check.check();
          results.push({ name: check.name, ok: true });
        } catch (error) {
          results.push({
            name: check.name,
            ok: false,
            detail: error instanceof Error ? error.message : String(error),
          });
        }
      }
      const ready = results.every((result) => result.ok);
      const body: ReadinessResponseBody = {
        status: ready ? "ready" : "not_ready",
        service: "signals-platform",
        checks: results,
      };
      await reply.code(ready ? 200 : 503).send(body);
    },
  );

  if (options.db) {
    registerPlatformV1Routes(app, {
      db: options.db,
      requireApiKey: createApiKeyGuard(options.apiKeys ?? []),
      marketData: options.marketData ?? { url: "", apiKey: "" },
    });
  }

  return app;
}
