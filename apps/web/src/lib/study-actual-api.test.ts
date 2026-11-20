import { afterEach, expect, it, vi } from "vitest";
import { actualCountsFixture, notCalculatedActualResponse, studyActualFixture } from "../../test/study-actual-fixture";
import { SESSION_EXPIRED_EVENT } from "./platform-api";
import { getStudyActualSummary, parseStudyActualSummary, refreshStudyActual, StudyActualApiError } from "./study-actual-api";

afterEach(() => { vi.restoreAllMocks(); vi.unstubAllGlobals(); });
const json = (value: unknown, status = 200) => new Response(JSON.stringify(value), { status });

it("reads persisted state without a calculating POST and projects away full report/setup payloads", async () => {
  const summary = studyActualFixture();
  const fetch = vi.fn().mockResolvedValue(json({ ...summary, report: { sourceArtifactId: null }, setups: ["not retained"] }));
  vi.stubGlobal("fetch", fetch);
  const signal = new AbortController().signal;
  expect(await getStudyActualSummary(49, signal)).toEqual(summary);
  const [path, init] = fetch.mock.calls[0] as [string, RequestInit];
  expect(path).toBe("/v1/optimization-studies/49/actual");
  expect(init.method).toBeUndefined(); expect(init.body).toBeUndefined(); expect(init.signal).toBe(signal);
  expect(init.credentials).toBe("same-origin"); expect(new Headers(init.headers).get("x-platform-request")).toBe("1");
});

it("submits only an empty manual refresh command and preserves the server's frozen cutoff and job identity", async () => {
  const queued = studyActualFixture(49, { jobId: "job-49-1", revision: 1, status: "queued", stage: "queued", observedThroughUnix: 1795201200 });
  const fetch = vi.fn().mockResolvedValue(json(queued, 202)); vi.stubGlobal("fetch", fetch);
  expect(await refreshStudyActual(49, new AbortController().signal)).toEqual(queued);
  const [path, init] = fetch.mock.calls[0] as [string, RequestInit];
  expect(path).toBe("/v1/optimization-studies/49/actual/refresh"); expect(init.method).toBe("POST"); expect(init.body).toBe("{}");
  expect(new Headers(init.headers).get("content-type")).toBe("application/json");
});

it("accepts the exact backend not_calculated detail envelope with null stage before the first manual refresh", async () => {
  const queued = studyActualFixture(49, { revision: 1, jobId: "first-job", status: "queued", stage: "queued" });
  const fetch = vi.fn().mockResolvedValueOnce(json(notCalculatedActualResponse())).mockResolvedValueOnce(json(queued, 202));
  vi.stubGlobal("fetch", fetch);
  const idle = await getStudyActualSummary(49, new AbortController().signal);
  expect(idle).toEqual(studyActualFixture()); expect(idle.stage).toBeNull(); expect(idle.eligibility.eligible).toBe(true);
  expect(await refreshStudyActual(49, new AbortController().signal)).toEqual(queued);
  expect(fetch).toHaveBeenCalledTimes(2);
});

it("keeps explicit closed/all counts and nullable mean, never deriving either from gates or evaluated rows", () => {
  const actual = studyActualFixture(49, { status: "completed", stage: "completed", counts: { total: 23, closed: 0, tp: 0, sl: 0, gateFailed: 20, unclosed: 3, noData: 0 } });
  expect(parseStudyActualSummary({ ...actual, progress: { evaluatedSetups: 23 }, report: null }, 49)).toEqual(actual);
  expect(parseStudyActualSummary(studyActualFixture(49, { status: "completed", stage: "completed", counts: actualCountsFixture, meanR: 0 }), 49).meanR).toBe(0);
});

it.each([
  { revision: -1 }, { status: "success" }, { studyId: 50 }, { observedThroughUnix: NaN },
  { counts: { ...actualCountsFixture, closed: 177 } }, { counts: { ...actualCountsFixture, noData: -1 } },
  { counts: { ...actualCountsFixture, closed: 0 }, meanR: 0 },
  { status: "queued", stage: null }, { status: "running", stage: null },
  { status: "completed", stage: null }, { status: "failed", stage: null }, { stage: undefined },
])("rejects malformed compact actual state: %j", (patch) => {
  expect(() => parseStudyActualSummary({ ...studyActualFixture(), ...patch }, 49)).toThrow(StudyActualApiError);
});

it("exposes the existing job on a same-study busy response without issuing another request", async () => {
  const current = studyActualFixture(49, { revision: 2, jobId: "existing", status: "running", stage: "replaying" });
  const fetch = vi.fn().mockResolvedValue(json({ error: "study_actual_busy", actual: current }, 409)); vi.stubGlobal("fetch", fetch);
  await expect(refreshStudyActual(49, new AbortController().signal)).rejects.toMatchObject({ status: 409, code: "study_actual_busy", current });
  expect(fetch).toHaveBeenCalledTimes(1);
});

it.each([
  [422, "study_actual_ineligible", "execution_start_unavailable", /execution_start_unavailable/],
  [429, "resource_limit", null, /вычислительных ресурсов/],
  [404, "not_found", null, /недоступно или уже удалено/],
  [409, "study_actual_revision_changed", null, /текущую ревизию/],
] as const)("preserves typed rejection %i/%s", async (status, error, reason, message) => {
  vi.stubGlobal("fetch", vi.fn().mockResolvedValue(json({ error, reason }, status)));
  const result = refreshStudyActual(49, new AbortController().signal);
  await expect(result).rejects.toMatchObject({ status, code: error });
  await expect(result).rejects.toThrow(message);
});

it("uses the shared expired-session boundary for unauthorized commands", async () => {
  const listener = vi.fn(); window.addEventListener(SESSION_EXPIRED_EVENT, listener);
  try {
    vi.stubGlobal("fetch", vi.fn().mockResolvedValue(json({ error: "unauthorized" }, 401)));
    await expect(refreshStudyActual(49, new AbortController().signal)).rejects.toMatchObject({ status: 401, message: "Сессия истекла. Войдите заново." });
    expect(listener).toHaveBeenCalledOnce();
  } finally { window.removeEventListener(SESSION_EXPIRED_EVENT, listener); }
});
