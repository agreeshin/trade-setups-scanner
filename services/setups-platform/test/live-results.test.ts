import { afterAll, beforeAll, describe, expect, it } from "vitest";
import type { FastifyInstance } from "fastify";
import { Kysely, PostgresDialect } from "kysely";
import { Pool } from "pg";
import { buildApp } from "../src/app.js";
import { createMigrator } from "../src/db/migrator.js";
import type { SignalsDB } from "../src/db/schema.js";
import { DEFAULT_SPIKE_PARAMS } from "../src/detector/price-volume-spike.js";

const databaseUrl = process.env.TEST_DATABASE_URL;
const headers = { "x-api-key": "live-page-test" };
interface Page { setups: { symbol: string; closePrice: string }[]; offset: number; limit: number; total: number; snapshotId: number }

describe.skipIf(!databaseUrl)("bounded live result pages", () => {
  let db: Kysely<SignalsDB>;
  let app: FastifyInstance;
  const row = {
    exchange_code: "page-test", category: "linear", algorithm_id: "price_volume_spike",
    algorithm_version: "1.1.1", params: DEFAULT_SPIKE_PARAMS,
    open_time: new Date("2026-01-01T00:00:00Z"), close_time: new Date("2026-01-01T00:01:00Z"),
    close_price: "0.000000000123456789", notified_at: null,
  };
  beforeAll(async () => {
    db = new Kysely<SignalsDB>({ dialect: new PostgresDialect({ pool: new Pool({ connectionString: databaseUrl }) }) });
    const migration = await createMigrator(db).migrateToLatest();
    if (migration.error) throw new Error("Test database migration failed", { cause: migration.error });
    await db.deleteFrom("live_setups").where("exchange_code", "=", "page-test").execute();
    await db.insertInto("live_setups").values(Array.from({ length: 601 }, (_, index) => ({ ...row,
      symbol: index === 0 ? "ARCHIVE_100%_USDT" : `PAGE${index.toString().padStart(4, "0")}`,
    }))).execute();
    app = await buildApp({ db, apiKeys: [headers["x-api-key"]], marketData: { url: "http://127.0.0.1:1", apiKey: "unused" } });
  });
  afterAll(async () => {
    await app?.close();
    if (db) {
      await db.deleteFrom("live_setups").where("exchange_code", "=", "page-test").execute();
      await db.destroy();
    }
  });
  const get = async (query = "") => {
    const response = await app.inject({ url: `/v1/setups?q=page-test${query}`, headers });
    expect(response.statusCode).toBe(200);
    return response.json<Page>();
  };

  it("reaches rows beyond 500 with deterministic equal-time ordering", async () => {
    const first = await get();
    expect(first).toMatchObject({ limit: 500, offset: 0, total: 601 });
    expect(first.setups).toHaveLength(500);
    expect(first.setups[0]).toMatchObject({ symbol: "PAGE0600", closePrice: row.close_price });
    const last = await get(`&limit=100&offset=600&snapshotId=${first.snapshotId}`);
    expect(last).toMatchObject({ total: 601, offset: 600, limit: 100 });
    expect(last.setups.map((setup) => setup.symbol)).toEqual(["ARCHIVE_100%_USDT"]);
    expect((await get("&offset=10000&limit=25")).setups).toEqual([]);
  });

  it("searches the complete dataset as a literal case-insensitive substring", async () => {
    for (const query of ["archive", "100%", "_100", "  ArChIvE  "]) {
      const response = await app.inject({ url: `/v1/setups?limit=25&q=${encodeURIComponent(query)}`, headers });
      expect(response.statusCode).toBe(200);
      expect(response.json<Page>().setups.map((setup) => setup.symbol)).toEqual(["ARCHIVE_100%_USDT"]);
      expect(response.json<Page>().total).toBe(1);
    }
    const empty = await app.inject({ url: `/v1/setups?q=${encodeURIComponent("' OR 1=1 --")}`, headers });
    expect(empty.statusCode).toBe(200);
    expect(empty.json<Page>()).toMatchObject({ total: 0, setups: [] });
  });

  it("keeps later arrivals outside an existing page sequence until refresh", async () => {
    const first = await get("&limit=25");
    await db.insertInto("live_setups").values([
      { ...row, symbol: "NEW", close_time: new Date("2026-02-01T00:01:00Z") },
      { ...row, symbol: "LATE", close_time: new Date("2025-12-01T00:01:00Z") },
    ]).execute();
    const second = await get(`&limit=25&offset=25&snapshotId=${first.snapshotId}`);
    expect(second.total).toBe(601);
    expect(second.setups[0]?.symbol).toBe("PAGE0575");
    expect((await get(`&limit=25&snapshotId=${first.snapshotId}`)).setups).toEqual(first.setups);
    const refreshed = await get("&limit=25");
    expect(refreshed.total).toBe(603);
    expect(refreshed.setups[0]?.symbol).toBe("NEW");
  });

  it("preserves auth and rejects unbounded or invalid requests", async () => {
    expect((await app.inject({ url: "/v1/setups" })).statusCode).toBe(401);
    for (const query of ["limit=0", "limit=501", "offset=-1", "snapshotId=-1", `q=${"x".repeat(201)}`]) {
      expect((await app.inject({ url: `/v1/setups?${query}`, headers })).statusCode).toBe(400);
    }
  });
});
