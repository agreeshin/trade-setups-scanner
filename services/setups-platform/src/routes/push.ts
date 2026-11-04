import type { FastifyInstance, FastifyReply, FastifyRequest } from "fastify";
import type { Kysely } from "kysely";
import { Type } from "@sinclair/typebox";
import type { PushSubscription } from "web-push";
import type { SignalsDB } from "../db/schema.js";
import { getVapidKeys, savePushSubscription, validPushEndpoint, validPushSubscription, type PushSubscriptionsTable } from "../web-push.js";

export function registerPushRoutes(app: FastifyInstance, options: {
  db: Kysely<SignalsDB>;
  requireApiKey: (request: FastifyRequest, reply: FastifyReply) => Promise<void>;
}): void {
  const { db, requireApiKey } = options;
  const pushDb = db.withTables<{ push_subscriptions: PushSubscriptionsTable }>();
  app.get("/v1/push", { preHandler: requireApiKey }, async (_request, reply) => {
    reply.header("Cache-Control", "no-store");
    const { publicKey } = await getVapidKeys(db);
    const count = await pushDb.selectFrom("push_subscriptions").select((eb) => eb.fn.countAll<string>().as("count")).executeTakeFirstOrThrow();
    return { publicKey, subscriptions: Number(count.count), maxSubscriptions: 100 };
  });
  app.post<{ Body: PushSubscription }>("/v1/push/subscriptions", {
    preHandler: requireApiKey, bodyLimit: 4096,
    schema: { body: Type.Object({
      endpoint: Type.String({ maxLength: 2048 }),
      keys: Type.Object({ p256dh: Type.String({ maxLength: 100 }), auth: Type.String({ maxLength: 32 }) }),
    }) },
  }, async (request, reply) => {
    if (!validPushSubscription(request.body)) return reply.code(400).send({ error: "invalid_push_subscription" });
    if (!await savePushSubscription(db, request.body)) return reply.code(409).send({ error: "subscription_limit" });
    return reply.code(204).send();
  });
  app.delete<{ Body: { endpoint: string } }>("/v1/push/subscriptions", {
    preHandler: requireApiKey, bodyLimit: 4096,
    schema: { body: Type.Object({ endpoint: Type.String({ maxLength: 2048 }) }) },
  }, async (request, reply) => {
    if (!validPushEndpoint(request.body.endpoint)) return reply.code(400).send({ error: "invalid_push_endpoint" });
    await pushDb.deleteFrom("push_subscriptions").where("endpoint", "=", request.body.endpoint).execute();
    return reply.code(204).send();
  });
}
