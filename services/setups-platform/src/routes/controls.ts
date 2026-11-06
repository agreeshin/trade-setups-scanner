import type { FastifyInstance } from "fastify";
import { Type } from "@sinclair/typebox";
import type { PlatformV1Options } from "./v1.js";
import { getAlgorithm, saveAlgorithm, SpikeParamsSchema, validateSpikeParams } from "../algorithms.js";
import type { SpikeParams } from "../detector/price-volume-spike.js";
import { getSetting, setSetting } from "../settings.js";
import { fetchCatalog } from "../platform-catalog.js";
import { registerPushRoutes } from "./push.js";

export function registerControls(app: FastifyInstance, options: PlatformV1Options): void {
  const { db, requireApiKey, marketData } = options;
  const protectedRoute = { preHandler: [requireApiKey] };
  registerPushRoutes(app, { db, requireApiKey });
  app.get("/v1/algorithms", protectedRoute, async () => ({ algorithms: [await getAlgorithm(db)] }));
  app.get("/v1/export/config.json", protectedRoute, async (_request, reply) => {
    const algorithm = await getAlgorithm(db);
    const notifications = await getSetting(db, "notifications");
    const live = await getSetting(db, "live");
    const replay = await getSetting(db, "replay");
    const lists = await db.selectFrom("instrument_lists").select(["list_type", "exchange_code", "category", "symbol"]).orderBy("id").limit(2000).execute();
    return reply.header("Content-Disposition", 'attachment; filename="platform-config.json"').send({
      formatVersion: 1,
      algorithms: algorithm ? [{ id: algorithm.id, version: algorithm.version, params: algorithm.params, enabled: algorithm.enabled }] : [],
      notifications: { maxAgeSeconds: notifications.maxAgeSeconds ?? 60 },
      live: { enabled: live.enabled === true }, replay: { paused: replay.paused === true },
      instrumentLists: lists,
    });
  });
  app.put("/v1/algorithms/:id", {
    ...protectedRoute,
    schema: {
      params: Type.Object({ id: Type.String() }),
      body: Type.Object({ params: SpikeParamsSchema, enabled: Type.Boolean() }, { additionalProperties: false }),
    },
  }, async (request, reply) => {
    const { id } = request.params as { id: string };
    if (!await getAlgorithm(db, id)) return reply.code(404).send({ error: "unknown_algorithm" });
    const body = request.body as { params: SpikeParams; enabled: boolean };
    const error = validateSpikeParams(body.params);
    if (error) return reply.code(400).send({ error });
    await saveAlgorithm(db, id, body.params, body.enabled);
    return getAlgorithm(db, id);
  });
  app.get("/v1/settings/ui", protectedRoute, async () => ({ value: await getSetting(db, "ui") }));
  app.put("/v1/settings/ui", {
    ...protectedRoute, bodyLimit: 16384,
    schema: { body: Type.Object({ value: Type.Record(Type.String({ maxLength: 64 }), Type.Unknown()) }, { additionalProperties: false }) },
  }, async (request) => {
    const { value } = request.body as { value: Record<string, unknown> };
    await setSetting(db, "ui", value);
    return { value };
  });
  app.get("/v1/settings/live", protectedRoute, async () => ({ enabled: (await getSetting(db, "live")).enabled === true }));
  app.put("/v1/settings/live", {
    ...protectedRoute, schema: { body: Type.Object({ enabled: Type.Boolean() }, { additionalProperties: false }) },
  }, async (request) => {
    const value = request.body as { enabled: boolean };
    await setSetting(db, "live", value);
    return value;
  });
  app.get("/v1/live/status", protectedRoute, async () => {
    const status = await getSetting(db, "live-status");
    return { enabled: (await getSetting(db, "live")).enabled === true, running: false, lastScanAt: null, lastError: null, ...status };
  });
  app.get("/v1/instruments", protectedRoute, async () => ({ instruments: await fetchCatalog(marketData.url, marketData.apiKey) }));
  const listParams = Type.Object({ kind: Type.Union([Type.Literal("live"), Type.Literal("history")]) });
  app.get("/v1/instrument-lists/:kind", { ...protectedRoute, schema: { params: listParams } }, async (request) => {
    const { kind } = request.params as { kind: string };
    const configured = (await getSetting(db, `instrument-list:${kind}`)).configured === true;
    const rows = await db.selectFrom("instrument_lists").where("list_type", "=", kind).selectAll().execute();
    return { configured, instruments: rows.map((row) => ({ exchange: row.exchange_code, category: row.category, symbol: row.symbol })) };
  });
  app.put("/v1/instrument-lists/:kind", {
    ...protectedRoute,
    schema: { params: listParams, body: Type.Object({ instruments: Type.Array(Type.Object({
      exchange: Type.String({ maxLength: 32 }), category: Type.String({ maxLength: 32 }), symbol: Type.String({ maxLength: 64 }),
    }), { maxItems: 1000 }) }, { additionalProperties: false }) },
  }, async (request, reply) => {
    const { kind } = request.params as { kind: string };
    const { instruments } = request.body as { instruments: { exchange: string; category: string; symbol: string }[] };
    const catalog = await fetchCatalog(marketData.url, marketData.apiKey);
    const eligible = new Set(catalog.filter((item) => kind === "history" || item.status === "active").map((item) => `${item.exchange}/${item.category}/${item.symbol}`));
    const keys = instruments.map((item) => `${item.exchange}/${item.category}/${item.symbol}`);
    if (keys.some((key) => !eligible.has(key)) || new Set(keys).size !== keys.length) return reply.code(400).send({ error: "unknown_inactive_or_duplicate_instruments" });
    await db.transaction().execute(async (trx) => {
      await trx.deleteFrom("instrument_lists").where("list_type", "=", kind).execute();
      if (instruments.length) await trx.insertInto("instrument_lists").values(instruments.map((item) => ({ list_type: kind, exchange_code: item.exchange, category: item.category, symbol: item.symbol }))).execute();
      await setSetting(trx, `instrument-list:${kind}`, { configured: true });
    });
    return { configured: true, instruments };
  });
  app.get("/v1/status", protectedRoute, async () => {
    try {
      const response = await fetch(`${marketData.url}/ready`, { signal: AbortSignal.timeout(5000) });
      return { platform: "ready", marketData: response.ok ? "ready" : "not_ready" };
    } catch {
      return { platform: "ready", marketData: "unreachable" };
    }
  });
  const historyRoutes = [
    { method: "GET", path: "/instruments/:instrumentId/coverage", params: Type.Object({ instrumentId: Type.Integer({ minimum: 1 }) }) },
    { method: "GET", path: "/instruments/coverage-summary" },
    { method: "GET", path: "/instruments/listing" },
    { method: "GET", path: "/backfill-jobs" },
    { method: "POST", path: "/backfill-jobs", body: Type.Object({ instrumentId: Type.Integer({ minimum: 1 }), fromUnix: Type.Integer({ minimum: 0 }), toUnix: Type.Integer({ minimum: 0 }) }, { additionalProperties: false }) },
    { method: "POST", path: "/backfill-jobs/batch", body: Type.Object({ instrumentIds: Type.Array(Type.Integer({ minimum: 1 }), { minItems: 1, maxItems: 1000, uniqueItems: true }), fromUnix: Type.Integer({ minimum: 0 }), toUnix: Type.Integer({ minimum: 0 }) }, { additionalProperties: false }) },
    { method: "POST", path: "/backfill-jobs/:id/cancel", params: Type.Object({ id: Type.Integer({ minimum: 1 }) }) },
    { method: "GET", path: "/collection-targets" },
    { method: "PUT", path: "/collection-targets/:instrumentId", params: Type.Object({ instrumentId: Type.Integer({ minimum: 1 }) }), body: Type.Object({ enabled: Type.Boolean() }, { additionalProperties: false }) },
  ] as const;
  for (const route of historyRoutes) {
    app.route({
      method: route.method, url: `/v1/history${route.path}`, ...protectedRoute,
      schema: { ...("params" in route ? { params: route.params } : {}), ...("body" in route ? { body: route.body } : {}) },
      handler: async (request, reply) => {
        const params = request.params as Record<string, number>;
        const path = route.path.replace(/:([a-zA-Z]+)/g, (_match, name: string) => String(params[name]));
        try {
          const response = await fetch(`${marketData.url}/v1${path}`, {
            method: route.method,
            headers: { "x-api-key": marketData.apiKey, "content-type": "application/json" },
            ...(request.body === undefined ? {} : { body: JSON.stringify(request.body) }),
            signal: AbortSignal.timeout(30000),
          });
          if (!response.ok) return reply.code(response.status >= 500 ? 502 : response.status).send({ error: "history_service_request_failed" });
          if (route.method === "PUT") await setSetting(db, `collection:${params.instrumentId}`, request.body as Record<string, unknown>);
          return reply.send(await response.json());
        } catch {
          return reply.code(502).send({ error: "history_service_unavailable" });
        }
      },
    });
  }
}
