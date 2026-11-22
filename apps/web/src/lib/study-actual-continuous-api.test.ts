import { afterEach, expect, it, vi } from "vitest";
import { actualContinuousFixture } from "../../test/study-actual-fixture";
import { parseStudyActualContinuous, studyActualContinuous, StudyActualContinuousApiError } from "./study-actual-continuous-api";

afterEach(() => { vi.restoreAllMocks(); vi.unstubAllGlobals(); });
const json = (value: unknown, status = 200) => new Response(JSON.stringify(value), { status });

it("reads the exact per-algorithm endpoint with the shared authenticated request boundary and no default write", async () => {
  const saved = actualContinuousFixture("price/volume");
  const fetch = vi.fn().mockResolvedValue(json({ ...saved, ownerId: "not-retained", opaque: ["not-retained"] }));
  vi.stubGlobal("fetch", fetch);
  const signal = new AbortController().signal;
  expect(await studyActualContinuous(saved.algorithmId, signal)).toEqual(saved);
  expect(fetch).toHaveBeenCalledOnce();
  const [path, init] = fetch.mock.calls[0] as [string, RequestInit];
  expect(path).toBe("/v1/optimization-studies/actual/continuous?algorithmId=price%2Fvolume");
  expect(init.signal).toBe(signal); expect(init.method).toBeUndefined(); expect(init.body).toBeUndefined();
  expect(init.credentials).toBe("same-origin"); expect(new Headers(init.headers).get("x-platform-request")).toBe("1");
});

it("writes only enabled/fromDate/timeZone/revision, never a client cutoff, cursor, expectedRevision or refresh POST", async () => {
  const update = { enabled: false, fromDate: "2026-11-01", timeZone: "Asia/Kathmandu", revision: 7 };
  const saved = actualContinuousFixture("spike", { ...update, revision: 8, fromBoundary: "2026-10-31T18:15:00Z" });
  const fetch = vi.fn().mockResolvedValue(json(saved)); vi.stubGlobal("fetch", fetch);
  const signal = new AbortController().signal;
  expect(await studyActualContinuous("spike", signal, update)).toEqual(saved);
  const [path, init] = fetch.mock.calls[0] as [string, RequestInit];
  expect(path).toBe("/v1/optimization-studies/actual/continuous?algorithmId=spike");
  expect(init.method).toBe("PUT"); expect(init.signal).toBe(signal);
  expect(JSON.parse(init.body as string)).toEqual(update);
  expect(Object.keys(JSON.parse(init.body as string) as object).sort()).toEqual(["enabled", "fromDate", "revision", "timeZone"]);
  expect(new Headers(init.headers).get("content-type")).toBe("application/json");
});

it("parses camelCase cursor, queue and the automatic running job without inventing a study result", () => {
  const saved = actualContinuousFixture("spike", { cursorStudyId: 47, cursorExecutionStartedAt: "2026-11-20T12:00:00.125Z", lastSkipReason: "source_unavailable",
    queue: { manualQueued: 2, automaticQueued: 1, running: { studyId: 49, jobId: "job-49", status: "running", requestOrigin: "automatic", algorithmId: "spike" } } });
  expect(parseStudyActualContinuous(saved, "spike")).toEqual(saved);
});

it.each([
  null, {}, { algorithmId: "other" }, { enabled: "true" }, { revision: 0 }, { revision: -1 }, { revision: 1.5 },
  { fromDate: "2026-02-30" }, { fromDate: "2026-2-02" }, { fromBoundary: "2026-11-22T00:00:00" }, { timeZone: "Not/AZone" },
  { nextDueAt: "tomorrow" }, { cursorStudyId: 0 }, { cursorExecutionStartedAt: "today" }, { lastSkipReason: "x".repeat(301) },
  { queue: { manualQueued: -1, automaticQueued: 0, running: null } },
  { queue: { manualQueued: 0, automaticQueued: 0, running: {} } },
])("rejects malformed or mismatched saved state: %j", (patch) => {
  const value = patch === null ? null : { ...actualContinuousFixture(), ...patch };
  // An empty top-level response is not a server default.
  expect(() => parseStudyActualContinuous(patch && Object.keys(patch).length === 0 ? patch : value, "spike")).toThrow(StudyActualContinuousApiError);
});

it("preserves a conflict as an error without retrying the mutation", async () => {
  const fetch = vi.fn().mockResolvedValue(json({ error: "study_actual_configuration_changed", current: actualContinuousFixture() }, 409));
  vi.stubGlobal("fetch", fetch);
  const result = studyActualContinuous("spike", new AbortController().signal, { enabled: false, fromDate: "2026-11-22", timeZone: "UTC", revision: 1 });
  await expect(result).rejects.toMatchObject({ status: 409 });
  await expect(result).rejects.toThrow("другом клиенте");
  expect(fetch).toHaveBeenCalledOnce();
});
