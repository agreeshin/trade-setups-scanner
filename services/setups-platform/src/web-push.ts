import { sql, type Generated, type Kysely } from "kysely";
import webpush, { type PushSubscription } from "web-push";
import type { SignalsDB } from "./db/schema.js";
import type { Notifier } from "./notifications.js";

export interface PushSubscriptionsTable {
  endpoint: string;
  p256dh: string;
  auth: string;
  created_at: Generated<Date>;
}
export const VAPID_SETTING_KEY = "internal.web-push.vapid";
const MAX_SUBSCRIPTIONS = 100;

export function validPushEndpoint(endpoint: string): boolean {
  if (endpoint.length > 2048) return false;
  try {
    const url = new URL(endpoint);
    const host = url.hostname;
    return url.protocol === "https:" && !url.username && !url.password && !url.port && !url.hash &&
      (host === "fcm.googleapis.com" || host === "updates.push.services.mozilla.com" ||
        host === "push.apple.com" || host.endsWith(".push.apple.com") ||
        host === "notify.windows.com" || host.endsWith(".notify.windows.com"));
  } catch { return false; }
}

export function validPushSubscription(subscription: PushSubscription): boolean {
  const { p256dh, auth } = subscription.keys;
  return validPushEndpoint(subscription.endpoint) && /^[A-Za-z0-9_-]{86,88}={0,2}$/.test(p256dh) &&
    Buffer.from(p256dh, "base64url").length === 65 && Buffer.from(p256dh, "base64url")[0] === 4 &&
    /^[A-Za-z0-9_-]{22}={0,2}$/.test(auth) && Buffer.from(auth, "base64url").length === 16;
}

export async function getVapidKeys(db: Kysely<SignalsDB>): Promise<{ publicKey: string; privateKey: string }> {
  let row = await db.selectFrom("settings").select("value").where("key", "=", VAPID_SETTING_KEY).executeTakeFirst();
  if (!row) {
    const keys = webpush.generateVAPIDKeys();
    await db.insertInto("settings").values({ key: VAPID_SETTING_KEY, value: { ...keys } })
      .onConflict((oc) => oc.column("key").doNothing()).execute();
    row = await db.selectFrom("settings").select("value").where("key", "=", VAPID_SETTING_KEY).executeTakeFirstOrThrow();
  }
  if (typeof row.value.publicKey !== "string" || typeof row.value.privateKey !== "string") {
    throw new Error("invalid stored Web Push signing configuration");
  }
  return { publicKey: row.value.publicKey, privateKey: row.value.privateKey };
}

export async function savePushSubscription(db: Kysely<SignalsDB>, subscription: PushSubscription): Promise<boolean> {
  if (!validPushSubscription(subscription)) throw new Error("invalid push subscription");
  const pushDb = db.withTables<{ push_subscriptions: PushSubscriptionsTable }>();
  return pushDb.transaction().execute(async (trx) => {
    // Serialize count+insert across processes so the persisted cap cannot race.
    await sql`select pg_advisory_xact_lock(730031)`.execute(trx);
    const existing = await trx.selectFrom("push_subscriptions").select("endpoint").where("endpoint", "=", subscription.endpoint).executeTakeFirst();
    if (!existing) {
      const count = await trx.selectFrom("push_subscriptions").select((eb) => eb.fn.countAll<string>().as("count")).executeTakeFirstOrThrow();
      if (Number(count.count) >= MAX_SUBSCRIPTIONS) return false;
    }
    const keys = { p256dh: subscription.keys.p256dh, auth: subscription.keys.auth };
    await trx.insertInto("push_subscriptions").values({ endpoint: subscription.endpoint, ...keys })
      .onConflict((oc) => oc.column("endpoint").doUpdateSet(keys)).execute();
    return true;
  });
}

export function createPushNotifier(
  db: Kysely<SignalsDB>,
  instrument: { exchangeCode: string; category: string; symbol: string },
  maxAgeSeconds: number,
): Notifier {
  return {
    kind: "web-push",
    async send(setup) {
      if (!Number.isFinite(maxAgeSeconds) || maxAgeSeconds <= 0) return;
      const remaining = () => Math.floor(maxAgeSeconds - (Date.now() / 1000 - setup.closeTimeUnix));
      if (Date.now() / 1000 < setup.closeTimeUnix || remaining() < 0) return;
      try {
        const keys = await getVapidKeys(db);
        const pushDb = db.withTables<{ push_subscriptions: PushSubscriptionsTable }>();
        const subscriptions = await pushDb.selectFrom("push_subscriptions").selectAll().limit(MAX_SUBSCRIPTIONS).execute();
        const payload = JSON.stringify({ symbol: instrument.symbol.slice(0, 80), timestamp: setup.closeTimeUnix * 1000,
          expiresAt: (setup.closeTimeUnix + maxAgeSeconds) * 1000 });
        // Fixed batches bound network concurrency and retained promises.
        for (let offset = 0; offset < subscriptions.length; offset += 5) {
          const results = await Promise.allSettled(subscriptions.slice(offset, offset + 5).map(async (row) => {
            const ttl = remaining();
            if (ttl < 0 || !validPushEndpoint(row.endpoint)) return;
            try {
              await webpush.sendNotification({ endpoint: row.endpoint, keys: { p256dh: row.p256dh, auth: row.auth } }, payload, {
                TTL: Math.min(ttl, 28 * 24 * 60 * 60), timeout: 5000,
                vapidDetails: { subject: "mailto:admin@example.invalid", ...keys },
              });
            } catch (error) {
              const status = typeof error === "object" && error !== null && "statusCode" in error ? error.statusCode : undefined;
              if (status === 404 || status === 410) {
                await pushDb.deleteFrom("push_subscriptions").where("endpoint", "=", row.endpoint).execute();
              } else {
                // Provider errors can contain credential-bearing endpoints: never serialize them.
                console.warn("[web-push] delivery failed", { status: typeof status === "number" ? status : "network" });
              }
            }
          }));
          if (results.some((result) => result.status === "rejected")) console.warn("[web-push] subscriber cleanup failed");
        }
      } catch {
        // Delivery cannot abort scanning/persistence of subsequent live setups.
        console.warn("[web-push] delivery unavailable");
      }
    },
  };
}
