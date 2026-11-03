import Fastify, { type FastifyInstance } from "fastify";
import swagger from "@fastify/swagger";
import { Type, type Static } from "@sinclair/typebox";
import { sql, type Kysely } from "kysely";
import type { MarketDataDB } from "./db/schema.js";
import { createApiKeyGuard } from "./auth.js";
import { registerV1Routes } from "./routes/v1.js";

export interface ReadinessCheck {
  name: string;
  check: () => Promise<void>;
}

export interface AppOptions {
  readinessChecks?: ReadinessCheck[];
  openApiEnabled?: boolean;
  db?: Kysely<MarketDataDB>;
  apiKeys?: string[];
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
  const app = Fastify({
    logger: {
      level: process.env.LOG_LEVEL ?? "info",
    },
  });

  if (options.openApiEnabled !== false) {
    await app.register(swagger, {
      openapi: {
        info: {
          title: "Market Data Service API",
          description:
            "Self-contained market history service for authorized clients. Unix timestamps are seconds; decimal values travel as strings.",
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
      service: "market-data",
      time: new Date().toISOString(),
    }),
  );

  const readinessChecks: ReadinessCheck[] = [...(options.readinessChecks ?? [])];
  if (options.db) {
    readinessChecks.push({
      name: "database",
      check: async () => {
        await sql`select 1`.execute(options.db!);
      },
    });
  }

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
        service: "market-data",
        checks: results,
      };
      await reply.code(ready ? 200 : 503).send(body);
    },
  );

  if (options.db) {
    registerV1Routes(app, {
      db: options.db,
      requireApiKey: createApiKeyGuard(options.apiKeys ?? []),
    });
  }

  return app;
}
