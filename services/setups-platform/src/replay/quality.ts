import type { Kysely } from "kysely";
import type { SignalsDB } from "../db/schema.js";
import type { PushResult, SkipReason } from "../detector/price-volume-spike.js";

export type QualityReason = "evaluated" | "missing_candle" | SkipReason;
export interface QualityInterval { fromUnix: number; toUnix: number; reason: QualityReason }
export interface ReplayQuality {
  version: 1;
  state: "in_progress" | "complete" | "partial" | "no_eligible_data";
  requiredHistoryMinutes: number;
  requestedMinutes: number;
  evaluatedMinutes: number;
  evaluatedWithGapsMinutes: number;
  skippedMinutes: number;
  unassessedMinutes: number;
  assessedThroughUnix: number;
  reasonCounts: Record<Exclude<QualityReason, "evaluated">, number>;
}

/** One tracker per calculation. Drain intervals after each bounded input page. */
export class ReplayQualityTracker {
  readonly quality: ReplayQuality;
  private readonly end: number;
  private intervals: QualityInterval[] = [];

  constructor(fromUnix: number, toUnix: number, requiredHistoryMinutes: number) {
    const start = Math.ceil(fromUnix / 60) * 60;
    this.end = Math.ceil(toUnix / 60) * 60;
    const requestedMinutes = (this.end - start) / 60;
    this.quality = { version: 1, state: "in_progress", requiredHistoryMinutes, requestedMinutes,
      evaluatedMinutes: 0, evaluatedWithGapsMinutes: 0, skippedMinutes: 0,
      unassessedMinutes: requestedMinutes, assessedThroughUnix: start,
      reasonCounts: { missing_candle: 0, insufficient_warmup: 0, recent_gap: 0,
        price_reference_missing: 0, gap_tolerance_exceeded: 0 } };
  }

  observe(openTimeUnix: number, result: PushResult): void {
    // Quality counts UTC minute slots; input before the requested grid is warmup only.
    const time = Math.floor(openTimeUnix / 60) * 60;
    if (time < this.quality.assessedThroughUnix || time >= this.end) return;
    this.append(time, "missing_candle");
    const reason = result.skipReason ?? (result.status === "warmup" ? "insufficient_warmup" : "evaluated");
    this.append(time + 60, reason);
    if (reason === "evaluated" && (result.missingMinutes ?? 0) > 0) this.quality.evaluatedWithGapsMinutes++;
  }

  finish(): void {
    this.append(this.end, "missing_candle");
    this.quality.state = this.quality.evaluatedMinutes === 0 ? "no_eligible_data"
      : this.quality.skippedMinutes > 0 ? "partial" : "complete";
  }

  drain(): QualityInterval[] {
    const intervals = this.intervals;
    this.intervals = [];
    return intervals;
  }

  private append(toUnix: number, reason: QualityReason): void {
    const fromUnix = this.quality.assessedThroughUnix;
    if (toUnix <= fromUnix) return;
    const minutes = (toUnix - fromUnix) / 60;
    if (reason === "evaluated") this.quality.evaluatedMinutes += minutes;
    else { this.quality.skippedMinutes += minutes; this.quality.reasonCounts[reason] += minutes; }
    this.quality.unassessedMinutes -= minutes;
    this.quality.assessedThroughUnix = toUnix;
    const last = this.intervals.at(-1);
    if (last?.reason === reason && last.toUnix === fromUnix) last.toUnix = toUnix;
    else this.intervals.push({ fromUnix, toUnix, reason });
  }
}

/** Called in the same transaction as the corresponding result-page checkpoint. */
export async function saveQualityPage(db: Kysely<SignalsDB>, runId: number,
  quality: ReplayQuality, intervals: QualityInterval[]): Promise<void> {
  if (intervals.length) {
    const first = intervals[0]!;
    const last = await db.selectFrom("replay_quality_intervals").selectAll().where("run_id", "=", runId)
      .orderBy("from_unix", "desc").limit(1).executeTakeFirst();
    let start = 0;
    if (last?.reason === first.reason && Number(last.to_unix) === first.fromUnix) {
      await db.updateTable("replay_quality_intervals").set({ to_unix: first.toUnix })
        .where("run_id", "=", runId).where("from_unix", "=", last.from_unix).execute();
      start = 1;
    }
    const additions = intervals.slice(start);
    if (additions.length) await db.insertInto("replay_quality_intervals").values(additions.map((interval) => ({
      run_id: runId, from_unix: interval.fromUnix, to_unix: interval.toUnix, reason: interval.reason,
    }))).execute();
  }
  await db.updateTable("replay_runs").set({ quality, quality_source_run_id: runId }).where("id", "=", runId).execute();
}
