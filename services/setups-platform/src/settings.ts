import type { Kysely } from "kysely";
import type { SignalsDB } from "./db/schema.js";

const NOTIFICATION_MAX_AGE_KEY = "notifications";
export const DEFAULT_NOTIFICATION_MAX_AGE_SECONDS = 60;

export async function getNotificationMaxAgeSeconds(
  db: Kysely<SignalsDB>,
): Promise<number> {
  const row = await db
    .selectFrom("settings")
    .where("key", "=", NOTIFICATION_MAX_AGE_KEY)
    .select("value")
    .executeTakeFirst();
  const raw = row?.value.maxAgeSeconds;
  return typeof raw === "number" && Number.isFinite(raw) && raw > 0
    ? raw
    : DEFAULT_NOTIFICATION_MAX_AGE_SECONDS;
}

export async function setNotificationMaxAgeSeconds(
  db: Kysely<SignalsDB>,
  seconds: number,
): Promise<void> {
  if (!Number.isFinite(seconds) || seconds <= 0) {
    throw new Error("notification max age must be a positive number of seconds");
  }
  await db
    .insertInto("settings")
    .values({ key: NOTIFICATION_MAX_AGE_KEY, value: { maxAgeSeconds: seconds } })
    .onConflict((oc) =>
      oc
        .column("key")
        .doUpdateSet({ value: { maxAgeSeconds: seconds }, updated_at: new Date() }),
    )
    .execute();
}
