import { afterAll, afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { randomBytes } from "node:crypto";
import Fastify from "fastify";
import webpush from "web-push";
import { createDb } from "../src/db/db.js";
import { up as initial } from "../src/db/migrations/001_initial.js";
import { up as pushMigration } from "../src/db/migrations/003_web_push.js";
import { createPushNotifier, getVapidKeys, savePushSubscription, validPushEndpoint, VAPID_SETTING_KEY, type PushSubscriptionsTable } from "../src/web-push.js";
import { registerPushRoutes } from "../src/routes/push.js";
import { createSessionAuth } from "../src/auth.js";
import { DEFAULT_SPIKE_PARAMS, SPIKE_ALGORITHM_ID, type SpikeSetup } from "../src/detector/price-volume-spike.js";

function subscription(id = "one") {
  return { endpoint: `https://fcm.googleapis.com/fcm/send/${id}`, keys: {
    p256dh: Buffer.concat([Buffer.from([4]), Buffer.alloc(64, 1)]).toString("base64url"),
    auth: Buffer.alloc(16, 2).toString("base64url"),
  } };
}
function setup(age = 5): SpikeSetup {
  return { algorithmId: SPIKE_ALGORITHM_ID, algorithmVersion: "1", closeTimeUnix: Date.now() / 1000 - age,
    openTimeUnix: Date.now() / 1000 - age - 60, closePrice: "100", params: DEFAULT_SPIKE_PARAMS };
}

describe("push provider allowlist", () => {
  it("accepts only HTTPS browser providers without custom ports or credentials", () => {
    for (const endpoint of ["https://fcm.googleapis.com/send/a", "https://updates.push.services.mozilla.com/a", "https://web.push.apple.com/a", "https://wns.notify.windows.com/a"]) expect(validPushEndpoint(endpoint)).toBe(true);
    for (const endpoint of ["http://fcm.googleapis.com/a", "https://fcm.googleapis.com:8443/a", "https://user@fcm.googleapis.com/a", "https://127.0.0.1/a", "https://evilpush.apple.com/a", "https://push.apple.com.evil.test/a", "https://evil.test/a", "https://fcm.googleapis.com/a#fragment"]) expect(validPushEndpoint(endpoint)).toBe(false);
  });
});

const databaseUrl = process.env.TEST_DATABASE_URL;
describe.skipIf(!databaseUrl)("persisted Web Push with fake sender", () => {
  const base = createDb(databaseUrl ?? "");
  const schema = `push_test_${randomBytes(6).toString("hex")}`;
  const db = base.withSchema(schema);
  const pushDb = db.withTables<{ push_subscriptions: PushSubscriptionsTable }>();
  beforeAll(async () => {
    await base.schema.createSchema(schema).execute();
    await initial(db);
    await pushMigration(db);
  });
  afterAll(async () => { await base.schema.dropSchema(schema).cascade().execute(); await base.destroy(); });
  beforeEach(async () => {
    await pushDb.deleteFrom("push_subscriptions").execute();
    await db.deleteFrom("settings").execute();
    vi.spyOn(webpush, "sendNotification").mockResolvedValue({ statusCode: 201, body: "", headers: {} });
  });
  afterEach(() => vi.restoreAllMocks());

  it("persists a single private keypair and exposes only public status behind auth", async () => {
    const keys = await Promise.all([getVapidKeys(db), getVapidKeys(db)]);
    expect(keys[0]).toEqual(keys[1]);
    const stored = await db.selectFrom("settings").selectAll().where("key", "=", VAPID_SETTING_KEY).execute();
    expect(stored).toHaveLength(1);
    const app = Fastify();
    const auth = createSessionAuth(["test-key"]);
    auth.register(app);
    registerPushRoutes(app, { db, requireApiKey: auth.requireApiKey });
    try {
      expect((await app.inject("/v1/push")).statusCode).toBe(401);
      const headers = { "x-api-key": "test-key" };
      const response = await app.inject({ url: "/v1/push", headers });
      expect(response.json()).toEqual({ publicKey: keys[0]?.publicKey, subscriptions: 0, maxSubscriptions: 100 });
      expect(response.body).not.toContain(keys[0]?.privateKey);
      expect((await app.inject({ method: "POST", url: "/v1/push/subscriptions", headers, payload: subscription() })).statusCode).toBe(204);
      expect((await app.inject({ method: "POST", url: "/v1/push/subscriptions", headers, payload: { ...subscription(), endpoint: "https://localhost/private" } })).statusCode).toBe(400);
      expect((await app.inject({ method: "DELETE", url: "/v1/push/subscriptions", headers, payload: { endpoint: subscription().endpoint } })).statusCode).toBe(204);
      expect(await pushDb.selectFrom("push_subscriptions").selectAll().execute()).toHaveLength(0);
      expect(webpush.sendNotification).not.toHaveBeenCalled();
    } finally { await app.close(); }
  });

  it("caps persisted subscriptions at 100 under concurrent writes", async () => {
    await pushDb.insertInto("push_subscriptions").values(Array.from({ length: 99 }, (_, i) => ({ endpoint: subscription(String(i)).endpoint, ...subscription().keys }))).execute();
    const results = await Promise.all([savePushSubscription(db, subscription("last-a")), savePushSubscription(db, subscription("last-b"))]);
    expect(results.filter(Boolean)).toHaveLength(1);
    expect(await pushDb.selectFrom("push_subscriptions").selectAll().execute()).toHaveLength(100);
    expect(await savePushSubscription(db, subscription("0"))).toBe(true);
  });

  it("checks wall-clock age, bounds TTL and timeout, removes expired subscriptions and isolates failures", async () => {
    for (const id of ["good", "gone", "missing", "failed"]) await savePushSubscription(db, subscription(id));
    const send = vi.mocked(webpush.sendNotification);
    send.mockImplementation((target) => {
      if (target.endpoint.endsWith("gone")) return Promise.reject(Object.assign(new Error("gone"), { statusCode: 410 }));
      if (target.endpoint.endsWith("missing")) return Promise.reject(Object.assign(new Error("missing"), { statusCode: 404 }));
      if (target.endpoint.endsWith("failed")) return Promise.reject(Object.assign(new Error("unavailable"), { statusCode: 503 }));
      return Promise.resolve({ statusCode: 201, body: "", headers: {} });
    });
    const notifier = createPushNotifier(db, { exchangeCode: "bybit", category: "linear", symbol: "BTCUSDT" }, 60);
    await notifier.send(setup(61));
    await notifier.send(setup(-10));
    expect(send).not.toHaveBeenCalled();
    await notifier.send(setup(10));
    expect(send).toHaveBeenCalledTimes(4);
    for (const [, payload, options] of send.mock.calls) {
      expect(options?.TTL).toBeLessThanOrEqual(50);
      expect(options?.TTL).toBeGreaterThanOrEqual(0);
      expect(options?.timeout).toBe(5000);
      expect(JSON.parse(String(payload))).toMatchObject({ symbol: "BTCUSDT" });
    }
    const rows = await pushDb.selectFrom("push_subscriptions").selectAll().execute();
    expect(rows).toHaveLength(2);
    expect(rows.some((row) => row.endpoint.endsWith("gone"))).toBe(false);
  });

  it("rechecks actual age between bounded batches", async () => {
    for (let i = 0; i < 6; i++) await savePushSubscription(db, subscription(String(i)));
    const now = Date.now();
    const clock = vi.spyOn(Date, "now").mockReturnValue(now);
    const send = vi.mocked(webpush.sendNotification);
    send.mockImplementation(() => {
      if (send.mock.calls.length === 5) clock.mockReturnValue(now + 70_000);
      return Promise.resolve({ statusCode: 201, body: "", headers: {} });
    });
    const notifier = createPushNotifier(db, { exchangeCode: "bybit", category: "linear", symbol: "BTCUSDT" }, 60);
    await notifier.send(setup(10));
    expect(send).toHaveBeenCalledTimes(5);
  });
});
