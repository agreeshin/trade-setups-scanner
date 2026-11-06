import { type Kysely, type Selectable } from "kysely";
import type { BackfillPlansTable, MarketDataDB } from "../db/schema.js";
import {
  ACTIVE_STATUSES,
  BackfillAdmissionError,
  DEFAULT_BACKFILL_ADMISSION_LIMITS,
  cancelBackfillRequest,
  requestBackfill,
  type BackfillAdmissionLimits,
} from "./backfill.js";

export interface BackfillPlanRequest {
  instrumentIds: number[];
  fromTs: Date;
  toTs: Date;
  requestedBy: string;
}

export interface BackfillPlanCounts {
  total: number;
  pending: number;
  enqueued: number;
  failed: number;
  cancelled: number;
}

export interface BackfillPlan {
  id: number;
  fromTs: Date;
  toTs: Date;
  status: string;
  requestedBy: string;
  createdAt: Date;
  updatedAt: Date;
  counts: BackfillPlanCounts;
  activeJobs: number;
}

export async function createBackfillPlan(
  db: Kysely<MarketDataDB>,
  request: BackfillPlanRequest,
  limits: BackfillAdmissionLimits = DEFAULT_BACKFILL_ADMISSION_LIMITS,
): Promise<number> {
  const planId = await db.transaction().execute(async (tx) => {
    const plan = await tx
      .insertInto("backfill_plans")
      .values({
        from_ts: request.fromTs,
        to_ts: request.toTs,
        status: "active",
        requested_by: request.requestedBy,
      })
      .returning("id")
      .executeTakeFirstOrThrow();
    const unique = [...new Set(request.instrumentIds)];
    await tx
      .insertInto("backfill_plan_items")
      .values(unique.map((instrumentId) => ({ plan_id: plan.id, instrument_id: instrumentId, status: "pending" as const })))
      .execute();
    return plan.id;
  });
  await topUpBackfillPlans(db, limits, planId);
  return planId;
}

/**
 * Enqueue pending plan items while admission limits allow it.
 * Temporary refusals (queue depth, database reserve) stop the run and are retried
 * on the next tick; per-item work-limit refusals mark the item failed and continue.
 */
export async function topUpBackfillPlans(
  db: Kysely<MarketDataDB>,
  limits: BackfillAdmissionLimits = DEFAULT_BACKFILL_ADMISSION_LIMITS,
  onlyPlanId?: number,
): Promise<{ enqueued: number; failed: number }> {
  let plans = await db
    .selectFrom("backfill_plans")
    .selectAll()
    .where("status", "=", "active")
    .orderBy("id", "asc")
    .execute();
  if (onlyPlanId !== undefined) plans = plans.filter((plan) => plan.id === onlyPlanId);
  let enqueued = 0;
  let failed = 0;
  for (const plan of plans) {
    const items = await db
      .selectFrom("backfill_plan_items")
      .selectAll()
      .where("plan_id", "=", plan.id)
      .where("status", "=", "pending")
      .orderBy("instrument_id", "asc")
      .execute();
    for (const item of items) {
      try {
        const jobId = await requestBackfill(
          db,
          { instrumentId: item.instrument_id, fromTs: plan.from_ts, toTs: plan.to_ts, requestedBy: plan.requested_by },
          limits,
        );
        await markPendingItem(db, plan.id, item.instrument_id, { status: "enqueued", job_id: jobId, error: null });
        enqueued += 1;
      } catch (error) {
        if (error instanceof BackfillAdmissionError && error.resource === "work_minutes") {
          await markPendingItem(db, plan.id, item.instrument_id, { status: "failed", error: `work_minutes_exceeded:${error.requested}:${error.limit}` });
          failed += 1;
          continue;
        }
        await finalizeDrainedPlan(db, plan.id);
        return { enqueued, failed };
      }
    }
    await finalizeDrainedPlan(db, plan.id);
  }
  return { enqueued, failed };
}

export async function cancelBackfillPlan(db: Kysely<MarketDataDB>, planId: number): Promise<boolean> {
  const plan = await db.selectFrom("backfill_plans").selectAll().where("id", "=", planId).executeTakeFirst();
  if (!plan) return false;
  await db.transaction().execute(async (tx) => {
    await tx.updateTable("backfill_plans").set({ status: "cancelled", updated_at: new Date() }).where("id", "=", planId).execute();
    await tx.updateTable("backfill_plan_items").set({ status: "cancelled", updated_at: new Date() })
      .where("plan_id", "=", planId).where("status", "=", "pending").execute();
  });
  const items = await db.selectFrom("backfill_plan_items").select("job_id")
    .where("plan_id", "=", planId).where("job_id", "is not", null).execute();
  for (const item of items) await cancelBackfillRequest(db, item.job_id!, plan.requested_by);
  return true;
}

export async function getBackfillPlan(db: Kysely<MarketDataDB>, id: number): Promise<BackfillPlan | null> {
  const rows = await db.selectFrom("backfill_plans").selectAll().where("id", "=", id).execute();
  if (rows.length === 0) return null;
  return (await attachItemStats(db, rows))[0] ?? null;
}

export async function listBackfillPlans(db: Kysely<MarketDataDB>): Promise<BackfillPlan[]> {
  const rows = await db.selectFrom("backfill_plans").selectAll().orderBy("id", "desc").execute();
  return attachItemStats(db, rows);
}

type PlanRow = Selectable<BackfillPlansTable>;

async function attachItemStats(db: Kysely<MarketDataDB>, plans: PlanRow[]): Promise<BackfillPlan[]> {
  const ids = plans.map((plan) => plan.id);
  const items = ids.length === 0 ? [] : await db
    .selectFrom("backfill_plan_items")
    .select(["plan_id", "status"])
    .where("plan_id", "in", ids)
    .execute();
  const activeJobRows = ids.length === 0 ? [] : await db
    .selectFrom("backfill_plan_items")
    .innerJoin("backfill_jobs", (join) => join.onRef("backfill_jobs.id", "=", "backfill_plan_items.job_id"))
    .where("backfill_plan_items.plan_id", "in", ids)
    .where("backfill_jobs.status", "in", ACTIVE_STATUSES)
    .select((eb) => ["backfill_plan_items.plan_id as plan_id", eb.fn.countAll<string>().as("n")])
    .groupBy("backfill_plan_items.plan_id")
    .execute();
  const activeJobs = new Map(activeJobRows.map((row) => [row.plan_id, Number(row.n)]));
  return plans.map((plan) => {
    const planItems = items.filter((item) => item.plan_id === plan.id);
    return {
      id: plan.id,
      fromTs: plan.from_ts,
      toTs: plan.to_ts,
      status: plan.status,
      requestedBy: plan.requested_by,
      createdAt: plan.created_at,
      updatedAt: plan.updated_at,
      counts: {
        total: planItems.length,
        pending: planItems.filter((item) => item.status === "pending").length,
        enqueued: planItems.filter((item) => item.status === "enqueued").length,
        failed: planItems.filter((item) => item.status === "failed").length,
        cancelled: planItems.filter((item) => item.status === "cancelled").length,
      },
      activeJobs: activeJobs.get(plan.id) ?? 0,
    };
  });
}

async function markPendingItem(
  db: Kysely<MarketDataDB>,
  planId: number,
  instrumentId: number,
  values: { status: "enqueued" | "failed" | "cancelled"; job_id?: number | null; error?: string | null },
): Promise<void> {
  await db.updateTable("backfill_plan_items").set({ ...values, updated_at: new Date() })
    .where((eb) => eb.and([
      eb("plan_id", "=", planId),
      eb("instrument_id", "=", instrumentId),
      eb("status", "=", "pending"),
    ]))
    .execute();
}

async function finalizeDrainedPlan(db: Kysely<MarketDataDB>, planId: number): Promise<void> {
  const pending = await db.selectFrom("backfill_plan_items").select("instrument_id")
    .where("plan_id", "=", planId).where("status", "=", "pending").limit(1).executeTakeFirst();
  if (pending === undefined) {
    await db.updateTable("backfill_plans").set({ status: "completed", updated_at: new Date() })
      .where((eb) => eb.and([eb("id", "=", planId), eb("status", "=", "active")]))
      .execute();
  }
}
