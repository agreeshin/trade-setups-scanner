import { afterEach, expect, it, vi } from "vitest";
import producerPagesJson from "../../test/study-actual-producer-pages.json?raw";
import { deleteOptimizationStudy, listOptimizationStudies, summarizeOptimizationStudy, type OptimizationStudy } from "./optimization-api";

type ProducerPhase = "idle" | "queued" | "running" | "completed" | "failed";
// Sanitized real cookie-authenticated HTTP list responses, captured on 2026-11-20.
// Preserve the wire bodies: never preparse them through the frontend fixture builders.
const producerPages = JSON.parse(producerPagesJson) as Record<ProducerPhase, {
  studies: { id: number; actual: Record<string, unknown> }[]; total: number; limit: number; offset: number;
}>;

afterEach(() => { vi.restoreAllMocks(); vi.unstubAllGlobals(); });

it("requests bounded paginated compact summaries and keeps top-level bestScore", async () => {
  const page = { studies: [{ id: 7, bestScore: 0.25, forecastBest: { forecastR: 0.8, meanR: -0.3, tpPct: 6, slPct: 4 } }], total: 42, limit: 10, offset: 20 };
  const fetch = vi.fn().mockResolvedValue(new Response(JSON.stringify(page)));
  vi.stubGlobal("fetch", fetch);
  const controller = new AbortController();
  expect(await listOptimizationStudies("price/drop", controller.signal, 10, 20)).toEqual(page);
  const [path, init] = fetch.mock.calls[0] as [string, RequestInit];
  const query = new URL(path, "http://localhost").searchParams;
  expect(Object.fromEntries(query)).toEqual({ limit: "10", offset: "20", algorithmId: "price/drop" });
  expect(init.signal).toBe(controller.signal);
  expect(new Headers(init.headers).get("x-platform-request")).toBe("1");
});

it("deletes by ID with browser mutation guard and never sends settings writes", async () => {
  const fetch = vi.fn().mockResolvedValue(new Response(JSON.stringify({ deleted: true, id: 7 })));
  vi.stubGlobal("fetch", fetch);
  expect(await deleteOptimizationStudy(7, new AbortController().signal)).toEqual({ deleted: true, id: 7 });
  expect(fetch.mock.calls[0]?.[0]).toBe("/v1/optimization-studies/7");
  const init = fetch.mock.calls[0]?.[1] as RequestInit;
  expect(init.method).toBe("DELETE");
  expect(new Headers(init.headers).get("x-platform-request")).toBe("1");
});

it("preserves an interim bestScore without retaining full mutation-response details", () => {
  const study = { id: 7, status: "cancel_requested", spec: {}, progress: { bestScore: 0.125 }, result: null, events: ["large-event"], error: null, createdAt: "now", updatedAt: "now" } as unknown as OptimizationStudy;
  const summary = summarizeOptimizationStudy(study);
  expect(summary.bestScore).toBe(0.125);
  expect(summary.forecastBest).toBeNull();
  expect(summary).not.toHaveProperty("result");
  expect(summary).not.toHaveProperty("events");
  expect(summarizeOptimizationStudy({ ...study, result: { best: { score: 0 }, progress: study.progress, reason: "cancelled" } } as OptimizationStudy).bestScore).toBe(0);
});

it("preserves the server forecast pair in a compact mutation summary without inferring it from score", () => {
  const forecastBest = { forecastR: 0, meanR: -0.75, tpPct: 6, slPct: 4 };
  const study = { id: 7, status: "completed", spec: {}, progress: { bestScore: 9 }, result: { best: { score: 8, matrix: [{ forecastRiskExpectancy: 100 }] } },
    forecastBest, events: ["large-event"], error: null, createdAt: "now", updatedAt: "now" } as unknown as OptimizationStudy;
  const summary = summarizeOptimizationStudy(study);
  expect(summary).toMatchObject({ bestScore: 8, forecastBest });
  expect(summary).not.toHaveProperty("result");
  expect(summary).not.toHaveProperty("events");
  expect(summarizeOptimizationStudy({ ...study, forecastBest: null }).forecastBest).toBeNull();
});

it.each(["idle", "queued", "running", "completed", "failed"] as const)("parses the real %s HTTP list producer with one bounded GET and no actual discovery", async (phase) => {
  const fetch = vi.fn().mockResolvedValue(new Response(JSON.stringify(producerPages[phase])));
  vi.stubGlobal("fetch", fetch);
  const controller = new AbortController();
  const page = await listOptimizationStudies("price_volume_spike", controller.signal, 100, 0);
  expect(page).toMatchObject({ total: 2, limit: 100, offset: 0 });
  expect(page.studies.map((study) => study.id)).toEqual([2, 1]);
  expect(page.studies.find((study) => study.id === 2)?.actual).toEqual({ studyId: 2, eligibility: { eligible: false, reason: "execution_start_unavailable" },
    jobId: null, revision: 0, status: "not_calculated", stage: null, observedThroughUnix: null, counts: null, meanR: null, error: null });
  const actual = page.studies.find((study) => study.id === 1)?.actual;
  expect(actual).toMatchObject({ studyId: 1, eligibility: { eligible: true, reason: null }, status: phase === "idle" ? "not_calculated" : phase,
    stage: phase === "idle" ? null : phase === "running" ? "evaluating" : phase,
    revision: phase === "idle" ? 0 : phase === "failed" ? 2 : 1, observedThroughUnix: phase === "idle" ? null : 1795189680 });
  if (phase === "completed") {
    expect(actual?.meanR).toBe(0);
    expect(actual?.counts).toEqual({ total: 2, closed: 2, tp: 1, sl: 1, gateFailed: 0, unclosed: 0, noData: 0 });
    expect(actual?.error).toBeNull();
  } else {
    expect(actual?.meanR).toBeNull(); expect(actual?.counts).toBeNull();
    expect(actual?.error).toEqual(phase === "failed" ? { code: "fixture-failure", stage: "compute", message: "fixture-failure" } : null);
  }
  expect(fetch).toHaveBeenCalledTimes(1);
  const [path, init] = fetch.mock.calls[0] as [string, RequestInit];
  expect(path).toBe("/v1/optimization-studies?limit=100&offset=0&algorithmId=price_volume_spike");
  expect(init.signal).toBe(controller.signal); expect(init.method ?? "GET").toBe("GET");
  expect(new Headers(init.headers).get("x-platform-request")).toBe("1");
});

it.each(["missing eligibility", "active null stage", "invalid counts"] as const)("keeps strict producer validation for %s instead of inferring a fallback", async (fault) => {
  const phase = fault === "invalid counts" ? "completed" : "queued";
  const page = structuredClone(producerPages[phase]);
  const row = page.studies.find((study) => study.id === 1)!;
  if (fault === "missing eligibility") delete row.actual.eligibility;
  if (fault === "active null stage") row.actual.stage = null;
  if (fault === "invalid counts") row.actual.counts = { total: 1, closed: 2, tp: 1, sl: 1, gateFailed: 0, unclosed: 0, noData: 0 };
  const fetch = vi.fn().mockResolvedValue(new Response(JSON.stringify(page)));
  vi.stubGlobal("fetch", fetch);
  await expect(listOptimizationStudies("price_volume_spike", new AbortController().signal, 100, 0)).rejects.toThrow("Некорректный ответ расчёта матожидания");
  expect(fetch).toHaveBeenCalledTimes(1);
});
