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
  return { sourceArtifactId: 901, retrospective: true, executionStartedAt: "2026-11-20T12:00:00.125Z", observedThroughUnix: actualCutoff,
    historical: { availability: { status: "available" }, daily: [
      { dayUnix: actualDay - 86400, plotAtUnix: actualDay - 86400, meanR: 0.25, closedCount: 4, tpCount: 2, slCount: 2 },
      { dayUnix: actualDay, plotAtUnix: actualDay, meanR: -0.5, closedCount: 3, tpCount: 1, slCount: 2 },
    ] }, actual: { daily: [{ dayUnix: actualDay, plotAtUnix: actualStart, meanR: 1.25, closedCount: 2, tpCount: 1, slCount: 1 }] },
    originalObjective: { version: 1, trendMode: "blend", trendBendDays: "linear", trendBlendLambda: 0.7,
      forecast: { method: "decayed_probability", version: 1, halfLifeDays: 14, kappa: 10, priorPolicy: "gross_break_even" } },
    projection: { availability: { status: "available" }, method: "decayed_probability", points: [{ atUnix: actualDay - 86400, value: 0.2 }, { atUnix: actualStart, value: 0.8 }, { atUnix: actualCutoff, value: 0.8 }], fittedOnPostStudyData: false },
    originalEndpoint: { meanR: { status: "available", value: 0.25 }, forecastR: { status: "available", value: 0.8 }, decisionR: { status: "available", value: 0.635 }, atUnix: actualStart }, ...patch };
}
export function actualDetailFixture(studyId = 49, patch: Partial<StudyActualDetail> = {}): StudyActualDetail {
  return { ...studyActualFixture(studyId, { status: "completed", stage: "completed", revision: 1, jobId: `actual-${studyId}-1`, counts: actualCountsFixture, meanR: 1.25, observedThroughUnix: actualCutoff }),
    report: actualReportFixture(), ...patch };
}
export function actualSetupFixture(id = 1, patch: Partial<StudyActualSetup> = {}): StudyActualSetup {
  return { id, symbol: `ACTUAL${id}USDT`, exchange: "bybit", slotId: null, studyId: 49, direction: "short", setupTimeUnix: actualDay + 13 * 3600,
    entryTimeUnix: actualDay + 13 * 3600 + 300, entryPrice: 100, gateThreshold: 98, tpPrice: 90, slPrice: 105,
    closeTimeUnix: actualDay + 14 * 3600, closePrice: 90, riskReturn: 2, status: "tp", ...patch };
}
