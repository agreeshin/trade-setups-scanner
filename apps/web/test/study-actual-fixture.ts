import type { StudyActualSummary } from "../src/lib/study-actual-api";

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
