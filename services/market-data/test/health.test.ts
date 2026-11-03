import { afterAll, beforeAll, describe, expect, it } from "vitest";
import type { FastifyInstance } from "fastify";
import { buildApp } from "../src/app.js";

describe("market-data service health endpoints", () => {
  describe("GET /health", () => {
    let app: FastifyInstance;
    beforeAll(async () => {
      app = await buildApp({ openApiEnabled: false });
    });
    afterAll(async () => {
      await app.close();
    });
    it('responds 200 with status "ok" and service name', async () => {
      const response = await app.inject({ method: "GET", url: "/health" });
      expect(response.statusCode).toBe(200);
      const body = response.json<{ status: string; service: string; time: string }>();
      expect(body.status).toBe("ok");
      expect(body.service).toBe("market-data");
      expect(body.time).toBeTruthy();
    });
  });

  describe("GET /ready", () => {
    it("responds 200 ready when all readiness checks pass", async () => {
      const app = await buildApp({
        openApiEnabled: false,
        readinessChecks: [{ name: "stub-check", check: () => Promise.resolve() }],
      });
      const response = await app.inject({ method: "GET", url: "/ready" });
      expect(response.statusCode).toBe(200);
      const body = response.json<{
        status: string;
        checks: { name: string; ok: boolean }[];
      }>();
      expect(body.status).toBe("ready");
      expect(body.checks).toEqual([{ name: "stub-check", ok: true }]);
      await app.close();
    });

    it("responds 503 not_ready with failure detail when a check fails", async () => {
      const app = await buildApp({
        openApiEnabled: false,
        readinessChecks: [
          {
            name: "failing-check",
            check: () => Promise.reject(new Error("dependency unreachable")),
          },
        ],
      });
      const response = await app.inject({ method: "GET", url: "/ready" });
      expect(response.statusCode).toBe(503);
      const body = response.json<{
        status: string;
        checks: { name: string; ok: boolean; detail?: string }[];
      }>();
      expect(body.status).toBe("not_ready");
      expect(body.checks[0]?.ok).toBe(false);
      expect(body.checks[0]?.detail).toContain("dependency unreachable");
      await app.close();
    });
  });
});
