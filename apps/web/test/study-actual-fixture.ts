import type { StudyActualDetail, StudyActualReport, StudyActualSetup, StudyActualSummary } from "../src/lib/study-actual-api";

export function studyActualFixture(studyId = 49, patch: Partial<StudyActualSummary> = {}): StudyActualSummary {
  return { studyId, eligibility: { eligible: true, reason: null }, jobId: null, revision: 0, status: "not_calculated", stage: null,
    observedThroughUnix: null, counts: null, meanR: null, error: null, ...patch };
}

export const actualCountsFixture = { total: 176, closed: 123, tp: 82, sl: 41, gateFailed: 20, unclosed: 30, noData: 3 };

/** Exact idle detail envelope from api/src/study_actual.rs::detail, including its null stage. */
export function notCalculatedActualResponse(studyId = 49) {
  return { studyId, eligibility: { eligible: true, reason: null }, jobId: null, revision: 0, status: "not_calculated", stage: null,
    executionStartedAt: null, fromUnix: null, observedThroughUnix: null, requestedAt: null, startedAt: null, completedAt: null,
    replayBatchId: null, progress: {}, counts: null, meanR: null, error: null, report: null };
}

export const actualDay = Date.parse("2026-11-20T00:00:00Z") / 1000;
export const actualStart = actualDay + 12 * 3600 + 0.125;
export const actualCutoff = actualDay + 18 * 3600;
export function actualReportFixture(patch: Partial<StudyActualReport> = {}): StudyActualReport {
  const historical = patch.historical ?? { availability: { status: "available" } as const, daily: [
    { dayUnix: actualDay - 86400, plotAtUnix: actualDay - 86400, meanR: 0.25, closedCount: 4, tpCount: 2, slCount: 2 },
    { dayUnix: actualDay, plotAtUnix: actualDay, meanR: -0.5, closedCount: 3, tpCount: 1, slCount: 2 },
  ] };
  const actual = patch.actual ?? { daily: [{ dayUnix: actualDay, plotAtUnix: actualStart, meanR: 1.25, closedCount: 2, tpCount: 1, slCount: 1 }] };
  const days = new Map<number, { sumR: number; closedCount: number; tpCount: number; slCount: number }>();
  for (const point of [...(historical.availability.status === "available" ? historical.daily : []), ...actual.daily]) {
    const previous = days.get(point.dayUnix) ?? { sumR: 0, closedCount: 0, tpCount: 0, slCount: 0 };
    days.set(point.dayUnix, { sumR: previous.sumR + (point.meanR ?? 0) * point.closedCount,
      closedCount: previous.closedCount + point.closedCount, tpCount: previous.tpCount + point.tpCount, slCount: previous.slCount + point.slCount });
  }
  return { sourceArtifactId: 901, retrospective: true, executionStartedAt: "2026-11-20T12:00:00.125Z", observedThroughUnix: actualCutoff,
    historical, actual,
    originalObjective: { version: 1, trendMode: "blend", trendBendDays: "linear", trendBlendLambda: 0.7,
      forecast: { method: "decayed_probability", version: 1, halfLifeDays: 14, kappa: 10, priorPolicy: "gross_break_even" } },
    projection: { availability: { status: "available" }, method: "decayed_probability", points: [{ atUnix: actualDay - 86400, value: 0.2 }, { atUnix: actualStart, value: 0.8 }, { atUnix: actualCutoff, value: 0.8 }], fittedOnPostStudyData: false },
    originalEndpoint: { meanR: { status: "available", value: 0.25 }, forecastR: { status: "available", value: 0.8 }, decisionR: { status: "available", value: 0.635 }, atUnix: actualStart },
    display: { timeZone: "UTC", startDayUnix: actualDay,
      daily: [...days].sort(([left], [right]) => left - right).map(([dayUnix, counts]) => ({ ...counts, dayUnix, date: new Date(dayUnix * 1000).toISOString().slice(0, 10), meanR: counts.closedCount === 0 ? null : counts.sumR / counts.closedCount })),
      historical: { availability: historical.availability, fidelity: historical.availability.status === "available" ? "exact" : "unavailable", complete: historical.availability.status === "available", sourceIdentity: "artifact:901" },
      actual: { availability: { status: "available" }, fidelity: "exact", complete: true } }, ...patch };
}
export function originalReportFixture(patch: Partial<StudyActualReport> = {}): StudyActualReport {
  const report = actualReportFixture({ actual: { daily: [] } });
  return { ...report, observedThroughUnix: Math.floor(actualStart),
    projection: { ...report.projection, points: report.projection.points.filter((point) => point.atUnix <= actualStart) },
    display: { ...report.display!, actual: { availability: { status: "unavailable", reason: "not_calculated" }, fidelity: "unavailable", complete: false } }, ...patch };
}
export function actualDetailFixture(studyId = 49, patch: Partial<StudyActualDetail> = {}): StudyActualDetail {
  return { ...studyActualFixture(studyId, { status: "completed", stage: "completed", revision: 1, jobId: `actual-${studyId}-1`, counts: actualCountsFixture, meanR: 1.25, observedThroughUnix: actualCutoff }),
    report: actualReportFixture(), originalReport: originalReportFixture(), ...patch };
}
/** PG display contract: Havana repeats 00:00 on Nov 1; the day begins at the FIRST one, 04:00Z. */
export function havanaActualDetailFixture(): StudyActualDetail {
  const day = Date.parse("2026-11-01T04:00:00Z") / 1000;
  const start = day + 15 * 60 + 0.125; const cutoff = day + 27 * 3600;
  const base = actualReportFixture(); const original = originalReportFixture();
  const executionStartedAt = new Date(start * 1000).toISOString();
  const historical = { ...base.historical, daily: base.historical.daily.map((point, index) => ({ ...point,
    dayUnix: day - 4 * 3600 - (1 - index) * 86400, plotAtUnix: day - 4 * 3600 - (1 - index) * 86400 })) };
  const projection = { ...base.projection, points: base.projection.points.map((point, index) => ({ ...point, atUnix: [day - 86400, start, cutoff][index]! })) };
  const display = { ...base.display!, timeZone: "America/Havana", startDayUnix: day,
    daily: base.display!.daily.map((point, index) => ({ ...point, dayUnix: day - (1 - index) * 86400, date: index === 0 ? "2026-10-31" : "2026-11-01" })) };
  return actualDetailFixture(49, { observedThroughUnix: cutoff, counts: { total: 2, closed: 2, tp: 1, sl: 1, gateFailed: 0, unclosed: 0, noData: 0 },
    report: { ...base, executionStartedAt, observedThroughUnix: cutoff, historical, projection,
      actual: { daily: [{ ...base.actual.daily[0]!, dayUnix: day - 4 * 3600, plotAtUnix: day + 30 * 60 }] },
      originalEndpoint: { ...base.originalEndpoint, atUnix: start }, display },
    originalReport: { ...original, executionStartedAt, observedThroughUnix: Math.floor(start), historical,
      originalEndpoint: { ...original.originalEndpoint, atUnix: start }, projection: { ...projection, points: projection.points.slice(0, 2) },
      display: { ...original.display!, timeZone: display.timeZone, startDayUnix: day,
        daily: original.display!.daily.map((point, index) => ({ ...point, dayUnix: display.daily[index]!.dayUnix, date: display.daily[index]!.date })) } } });
}
export function actualSetupFixture(id = 1, patch: Partial<StudyActualSetup> = {}): StudyActualSetup {
  return { id, symbol: `ACTUAL${id}USDT`, exchange: "bybit", slotId: null, studyId: 49, direction: "short", setupTimeUnix: actualDay + 13 * 3600,
    entryTimeUnix: actualDay + 13 * 3600 + 300, entryPrice: 100, gateThreshold: 98, tpPrice: 90, slPrice: 105,
    closeTimeUnix: actualDay + 14 * 3600, closePrice: 90, riskReturn: 2, status: "tp", ...patch };
}
