import { afterAll, beforeAll, beforeEach, describe, expect, it } from "vitest";
import type { FastifyInstance } from "fastify";
import { Kysely, PostgresDialect } from "kysely";
import { Pool } from "pg";
import { buildApp } from "../src/app.js";
import { createMigrator } from "../src/db/migrator.js";
import type { SignalsDB } from "../src/db/schema.js";
import { DEFAULT_SPIKE_PARAMS, type SpikeSetup } from "../src/detector/price-volume-spike.js";

const databaseUrl = process.env.TEST_DATABASE_URL;
const headers = { "x-api-key": "replay-results-test" };
const start = 1_700_000_000;

interface ReplayPage {
  run: { setupCount: number; params: unknown };
  setups: { openTimeUnix: number; details: unknown }[];
  offset: number;
  limit: number;
  total: number;
}

describe.skipIf(!databaseUrl)("bounded saved replay results", () => {
  let db: Kysely<SignalsDB>;
  let app: FastifyInstance;
  let id: number;
  let emptyId: number;
  const queries: string[] = [];
  beforeAll(async () => {
    db = new Kysely<SignalsDB>({
      dialect: new PostgresDialect({ pool: new Pool({ connectionString: databaseUrl!, max: 2 }) }),
      log(event) {
        if (queries.length === 100) queries.shift();
        queries.push(event.query.sql);
      },
    });
    const migrated = await createMigrator(db).migrateToLatest();
    if (migrated.error) throw new Error("migration failed", { cause: migrated.error });
    const setups: SpikeSetup[] = Array.from({ length: 1205 }, (_, index) => ({
      algorithmId: "price_volume_spike", algorithmVersion: "1.0.0", params: DEFAULT_SPIKE_PARAMS,
      openTimeUnix: start + index * 60, closeTimeUnix: start + (index + 1) * 60, closePrice: "112",
      ...(index === 0 ? { details: { growthPct: "12", volumeLongRatio: "3.01", volumeShortRatio: null,
        overLong: true, overShort: false, missingMinutes: 0 } } : {}),
    }));
    const row = { exchange_code: "bybit", category: "linear", symbol: '=SUM(1,2)"',
      from_ts: new Date(start * 1000), to_ts: new Date((start + 1205 * 60) * 1000),
      algorithm_id: "price_volume_spike", algorithm_version: "1.0.0", params: DEFAULT_SPIKE_PARAMS,
      status: "done", cache_hit: false };
    const inserted = await db.insertInto("replay_runs").values([
      { ...row, setups: JSON.stringify(setups) as unknown as SpikeSetup[] }, { ...row, setups: null },
    ]).returning("id").execute();
    id = inserted[0]!.id;
    emptyId = inserted[1]!.id;
    app = await buildApp({ db, apiKeys: [headers["x-api-key"]], marketData: { url: "http://127.0.0.1:1", apiKey: "unused" } });
  });
  beforeEach(() => { queries.length = 0; });
  afterAll(async () => {
    await app?.close();
    if (db) {
      if (id && emptyId) await db.deleteFrom("replay_runs").where("id", "in", [id, emptyId]).execute();
      await db.destroy();
    }
  });

  it("lists full setup counts without selecting saved arrays", async () => {
    const response = await app.inject({ url: "/v1/replay-runs", headers });
    expect(response.statusCode).toBe(200);
    const result = response.json<{ runs: { id: number; setupCount: number; setups?: unknown }[] }>();
    const run = result.runs.find((item) => item.id === id)!;
    expect(run.setupCount).toBe(1205);
    expect(run.setups).toBeUndefined();
    const query = queries.find((text) => text.includes('from "replay_runs"'))!;
    expect(query).toContain("jsonb_array_length(setups)");
    expect(query).not.toContain("select *");
    expect(query).not.toContain(', "setups"');
  });

  it("defaults to 100 results and slices selected pages within PostgreSQL", async () => {
    const response = await app.inject({ url: `/v1/replay-runs/${id}`, headers });
    expect(response.statusCode).toBe(200);
    const first = response.json<ReplayPage>();
    expect(first).toMatchObject({ offset: 0, limit: 100, total: 1205, run: { setupCount: 1205, params: DEFAULT_SPIKE_PARAMS } });
    expect(first.setups).toHaveLength(100);
    expect(first.setups[0]!.details).toMatchObject({ growthPct: "12" });
    expect(first.setups[1]!.details).toBeNull();
    const responsePage = await app.inject({ url: `/v1/replay-runs/${id}?offset=1000&limit=500`, headers });
    const page = responsePage.json<ReplayPage>();
    expect(page.setups).toHaveLength(205);
    expect(page.setups[0]!.openTimeUnix).toBe(start + 1000 * 60);
    expect(page.total).toBe(1205);
    expect(queries.filter((text) => text.includes("jsonb_path_query_array"))).toHaveLength(2);
    expect(queries.some((text) => text.includes("select *"))).toBe(false);
  });

  it("handles empty, past-end and invalid pages without loading original arrays", async () => {
    for (const url of [`/v1/replay-runs/${emptyId}`, `/v1/replay-runs/${id}?offset=2000`, `/v1/replay-runs/${id}?offset=9007199254740000`]) {
      const response = await app.inject({ url, headers });
      expect(response.statusCode).toBe(200);
      expect(response.json<ReplayPage>().setups).toHaveLength(0);
    }
    for (const query of ["offset=-1", "limit=0", "limit=501", "offset=1.5"]) {
      expect((await app.inject({ url: `/v1/replay-runs/${id}?${query}`, headers })).statusCode).toBe(400);
    }
    expect((await app.inject({ url: "/v1/replay-runs/2147483647", headers })).statusCode).toBe(404);
  });

  it("exports every saved result using bounded database pages and safe quoted text", async () => {
    const response = await app.inject({ url: `/v1/replay-runs/${id}/export.csv`, headers });
    expect(response.statusCode).toBe(200);
    expect(response.headers["content-type"]).toContain("text/csv");
    expect(response.headers["content-disposition"]).toContain(`replay-${id}.csv`);
    const lines = response.body.trimEnd().split("\r\n");
    expect(lines).toHaveLength(1206);
    const cells = (line: string) => [...line.matchAll(/"((?:[^"]|"")*)"(?=,|$)/gu)].map((match) => match[1]!.replaceAll('""', '"'));
    const first = cells(lines[1]!);
    expect(first).toHaveLength(16);
    expect(first.slice(0, 9)).toEqual(["WATCH", "bybit", "linear", '\'=SUM(1,2)"', "price_volume_spike", "1.0.0", String(start), String(start + 60), "112"]);
    expect(first.slice(9, 15)).toEqual(["12", "3.01", "", "true", "false", "0"]);
    expect(JSON.parse(first[15]!)).toEqual(DEFAULT_SPIKE_PARAMS);
    expect(cells(lines[2]!).slice(9, 15)).toEqual(["", "", "", "", "", ""]);
    expect(cells(lines[1205]!)[6]).toBe(String(start + 1204 * 60));
    expect(queries.filter((text) => text.includes("jsonb_path_query_array"))).toHaveLength(3);
    expect(queries.some((text) => text.includes("select *"))).toBe(false);
  });

  it("protects exports and returns a header for empty saved results", async () => {
    expect((await app.inject({ url: `/v1/replay-runs/${id}/export.csv` })).statusCode).toBe(401);
    expect((await app.inject({ url: "/v1/replay-runs/2147483647/export.csv", headers })).statusCode).toBe(404);
    const response = await app.inject({ url: `/v1/replay-runs/${emptyId}/export.csv`, headers });
    expect(response.statusCode).toBe(200);
    expect(response.body.trimEnd().split("\r\n")).toHaveLength(1);
  });
});
