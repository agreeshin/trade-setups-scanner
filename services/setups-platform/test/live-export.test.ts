import { afterAll, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import type { FastifyInstance } from "fastify";
import { Kysely, PostgresDialect, sql } from "kysely";
import { Pool } from "pg";
import { buildApp } from "../src/app.js";
import { createMigrator } from "../src/db/migrator.js";
import type { SignalsDB } from "../src/db/schema.js";
import { DEFAULT_SPIKE_PARAMS } from "../src/detector/price-volume-spike.js";

const databaseUrl = process.env.TEST_DATABASE_URL;
const headers = { "x-api-key": "live-export-test" };
const route = "/v1/export/setups.csv";

describe.skipIf(!databaseUrl)("complete streaming live CSV", () => {
  let db: Kysely<SignalsDB>;
  let pool: Pool;
  let app: FastifyInstance;
  let origin: string;
  let serverClosed: Promise<void> | undefined;
  const queries: string[] = [];
  const price = "0.000000000123456789";
  const pageQueries = () => queries.filter((query) => query.startsWith("select") && query.includes('"live_setups"') && query.includes("order by"));
  beforeAll(async () => {
    pool = new Pool({ connectionString: databaseUrl });
    db = new Kysely<SignalsDB>({ dialect: new PostgresDialect({ pool }), log: (event) => { if (event.level === "query") queries.push(event.query.sql); } });
    const migration = await createMigrator(db).migrateToLatest();
    if (migration.error) throw new Error("Test database migration failed", { cause: migration.error });
    await db.deleteFrom("live_setups").where("exchange_code", "=", "csv-export-test").execute();
    await sql`insert into live_setups
      (exchange_code, category, symbol, algorithm_id, algorithm_version, params, open_time, close_time, close_price)
      select 'csv-export-test', 'linear', 'CSV' || lpad(i::text, 6, '0'), 'price_volume_spike', '1.1.1',
        ${JSON.stringify(DEFAULT_SPIKE_PARAMS)}::jsonb,
        '2026-01-01'::timestamptz + (i / 3) * interval '1 minute',
        '2026-01-01'::timestamptz + (i / 3 + 1) * interval '1 minute', ${price}
      from generate_series(1, 50005) as i`.execute(db);
    app = await buildApp({ db, apiKeys: [headers["x-api-key"]], marketData: { url: "http://127.0.0.1:1", apiKey: "unused" } });
    app.addHook("onRequest", (request, reply, done) => {
      if (request.headers["x-test-cancel"] === "1") serverClosed = new Promise((resolve) => { reply.raw.once("close", resolve); });
      done();
    });
    origin = await app.listen({ port: 0, host: "127.0.0.1" });
  });
  beforeEach(() => { queries.length = 0; });
  afterAll(async () => {
    await app?.close();
    if (db) {
      await db.deleteFrom("live_setups").where("exchange_code", "=", "csv-export-test").execute();
      await db.destroy();
    }
  });

  it("exports beyond 50000 without unbounded SQL reads or losing decimal precision", async () => {
    const response = await app.inject({ url: route, headers });
    expect(response.statusCode).toBe(200);
    expect(response.headers["content-disposition"]).toContain('filename="live-setups.csv"');
    expect(response.headers["cache-control"]).toBe("no-store");
    expect(response.headers["x-accel-buffering"]).toBe("no");
    const lines = response.body.trimEnd().split(/\r?\n/);
    const own = lines.filter((line) => line.includes("csv-export-test"));
    expect(own.length).toBe(50005);
    expect(own[0]).toContain("CSV000001");
    expect(own.at(-1)).toContain("CSV050005");
    expect(own.at(-1)).toContain(price);
    expect(pageQueries().length).toBeGreaterThan(100);
    expect(pageQueries().every((query) => query.includes("limit") && !query.includes("offset") && !query.includes("select *"))).toBe(true);
  }, 30000);

  it("freezes later appends outside a real HTTP export", async () => {
    const response = await fetch(`${origin}${route}`, { headers });
    expect(response.status).toBe(200);
    await db.insertInto("live_setups").values({ exchange_code: "csv-export-test", category: "linear",
      symbol: "APPENDED_AFTER_START", algorithm_id: "price_volume_spike", algorithm_version: "1.1.1",
      params: DEFAULT_SPIKE_PARAMS, open_time: new Date("2026-01-01"), close_time: new Date("2026-01-01T00:01:00Z"),
      close_price: price, notified_at: null }).execute();
    expect((await response.text()).includes("APPENDED_AFTER_START")).toBe(false);
  }, 30000);

  it("stops page queries and releases the connection after HTTP cancellation", async () => {
    const controller = new AbortController();
    const response = await fetch(`${origin}${route}`, { headers: { ...headers, "x-test-cancel": "1" }, signal: controller.signal });
    const reader = response.body!.getReader();
    await reader.read();
    controller.abort();
    await reader.cancel().catch(() => undefined);
    reader.releaseLock();
    await serverClosed;
    await vi.waitFor(() => { expect(pool.idleCount).toBe(pool.totalCount); });
    const count = pageQueries().length;
    expect(count).toBeLessThan(10);
    await app.inject({ url: "/health" });
    expect(pageQueries()).toHaveLength(count);
  });

  it("supports native cookie downloads and safely escapes CSV cells", async () => {
    const symbol = '=SUM(1,2)"\nline';
    await db.insertInto("live_setups").values({ exchange_code: "csv-export-test", category: "linear", symbol,
      algorithm_id: "price_volume_spike", algorithm_version: "1.1.1", params: DEFAULT_SPIKE_PARAMS,
      open_time: new Date("2026-01-01"), close_time: new Date("2026-01-01T00:01:00Z"), close_price: price, notified_at: null }).execute();
    const login = await app.inject({ method: "POST", url: "/v1/session", headers: { "x-platform-request": "1" }, payload: { apiKey: headers["x-api-key"] } });
    expect(login.statusCode).toBe(200);
    const cookie = login.cookies.map((entry) => `${entry.name}=${entry.value}`).join("; ");
    const response = await app.inject({ url: route, headers: { cookie } });
    expect(response.statusCode).toBe(200);
    expect(response.body.includes('"\'=SUM(1,2)""\nline"')).toBe(true);
    expect(response.body.includes(`"${price}"`)).toBe(true);
    expect((await app.inject({ url: route })).statusCode).toBe(401);
  }, 30000);
});
