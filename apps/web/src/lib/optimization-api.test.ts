import { afterEach, expect, it, vi } from "vitest";
import { deleteOptimizationStudy, listOptimizationStudies, summarizeOptimizationStudy, type OptimizationStudy } from "./optimization-api";

afterEach(() => { vi.restoreAllMocks(); vi.unstubAllGlobals(); });

it("requests bounded paginated compact summaries and keeps top-level bestScore", async () => {
  const page = { studies: [{ id: 7, bestScore: 0.25 }], total: 42, limit: 10, offset: 20 };
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
  expect(summary).not.toHaveProperty("result");
  expect(summary).not.toHaveProperty("events");
  expect(summarizeOptimizationStudy({ ...study, result: { best: { score: 0 }, progress: study.progress, reason: "cancelled" } } as OptimizationStudy).bestScore).toBe(0);
});
