import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { getOptimizationStudy, listOptimizationStudies, summarizeOptimizationStudy, type OptimizationStudy } from "./optimization-api";
import { SESSION_EXPIRED_EVENT } from "./platform-api";
import { fetchStudyPresentationPng, parseCurrentStudyPresentation, type CurrentStudyPresentation } from "./study-presentation-api";

const snapshot = { revision: 7, source: "actual", sourceRevision: 3, observationCutoff: 1795201200, publishedAt: "2026-11-20T21:41:00Z",
  meanR: 0.25, counts: { closed: 4, tp: 2, sl: 2 }, selectedObjectiveR: 0.4, winRate: 50 } as const;
const presentation = { status: "available", snapshot } as const;
const json = (body: unknown, status = 200) => new Response(JSON.stringify(body), { status });
beforeEach(() => { vi.stubGlobal("fetch", vi.fn()); });
afterEach(() => { vi.restoreAllMocks(); vi.unstubAllGlobals(); });

it.each([undefined, null, { status: "unavailable", reason: "original_archive_missing" }, presentation])("preserves additive presentation %j in list, detail and compact mutation projection", async (currentPresentation) => {
  const study = { id: 7, status: "completed", progress: { bestScore: 0.1 }, result: null, events: ["large"],
    ...(currentPresentation === undefined ? {} : { currentPresentation }) } as unknown as OptimizationStudy;
  vi.mocked(fetch).mockResolvedValueOnce(json({ studies: [study], limit: 20, offset: 0, total: 1 })).mockResolvedValueOnce(json(study));
  const signal = new AbortController().signal;
  expect((await listOptimizationStudies("spike", signal, 20)).studies[0]?.currentPresentation).toEqual(currentPresentation);
  expect((await getOptimizationStudy(7, signal)).currentPresentation).toEqual(currentPresentation);
  const summary = summarizeOptimizationStudy(study);
  expect(summary.currentPresentation).toEqual(currentPresentation);
  expect(summary).not.toHaveProperty("result"); expect(summary).not.toHaveProperty("events");
});

it("accepts an initial-only publication and explicitly unavailable statistics without inventing zeroes", () => {
  expect(parseCurrentStudyPresentation({ status: "available", snapshot: { ...snapshot, source: "initial", sourceRevision: null,
    meanR: null, counts: null, selectedObjectiveR: null, winRate: null } })).toMatchObject({ status: "available", snapshot: {
    source: "initial", sourceRevision: null, meanR: null, counts: null, selectedObjectiveR: null, winRate: null,
  } });
});

it.each([
  { revision: 0 }, { revision: 1.5 }, { source: "other" }, { sourceRevision: null },
  { source: "initial", sourceRevision: 3 }, { observationCutoff: -1 }, { publishedAt: "not-a-date" },
  { publishedAt: "2026-11-20T21:41:00" }, { meanR: Infinity }, { counts: { ...snapshot.counts, closed: 5 } },
  { counts: { ...snapshot.counts, closed: 0, tp: 0, sl: 0 } }, { winRate: 101 }, { selectedObjectiveR: NaN },
])("rejects malformed snapshot metadata %j rather than displaying mismatched provenance", (patch) => {
  expect(() => parseCurrentStudyPresentation({ ...presentation, snapshot: { ...snapshot, ...patch } })).toThrow("Некорректный сохранённый PNG-снимок");
});

it("strips unknown/heavy additive fields from compact presentation metadata", () => {
  const parsed = parseCurrentStudyPresentation({ ...presentation, snapshot: { ...snapshot, counts: { ...snapshot.counts, total: 4 }, daily: ["large"], report: { data: "large" } } });
  expect(parsed).toEqual(presentation);
});

it("performs only an authenticated revision-bound GET and returns bytes without allocating a URL", async () => {
  vi.mocked(fetch).mockResolvedValue(new Response(new Uint8Array([137, 80, 78, 71]), { headers: { "content-type": "image/png" } }));
  const signal = new AbortController().signal;
  const blob = await fetchStudyPresentationPng(7, 3, signal);
  expect(blob.size).toBe(4); expect(blob.type).toBe("image/png");
  expect(fetch).toHaveBeenCalledOnce();
  const [path, init] = vi.mocked(fetch).mock.calls[0]!;
  expect(path).toBe("/v1/optimization-studies/7/presentation.png?revision=3");
  expect(init).toMatchObject({ signal, credentials: "same-origin" }); expect(init?.method ?? "GET").toBe("GET");
  expect(new Headers(init?.headers).get("x-platform-request")).toBe("1");
});

it.each([
  [409, "study_presentation_revision_changed", "PNG-снимок обновился"],
  [404, "study_presentation_unavailable", "Сохранённый PNG исследования недоступен"],
  [500, "internal", "HTTP 500"],
] as const)("exposes HTTP %s as an explicit read error without retrying", async (status, error, message) => {
  vi.mocked(fetch).mockResolvedValue(json({ error }, status));
  const result = fetchStudyPresentationPng(7, 3, new AbortController().signal);
  await expect(result).rejects.toMatchObject({ status, code: error });
  await expect(result).rejects.toThrow(message);
  expect(fetch).toHaveBeenCalledOnce();
});

it("preserves the shared authentication-expiration boundary", async () => {
  const expired = vi.fn(); window.addEventListener(SESSION_EXPIRED_EVENT, expired);
  try {
    vi.mocked(fetch).mockResolvedValue(json({}, 401));
    await expect(fetchStudyPresentationPng(7, 3, new AbortController().signal)).rejects.toThrow("Сессия истекла");
    expect(expired).toHaveBeenCalledOnce();
  } finally { window.removeEventListener(SESSION_EXPIRED_EVENT, expired); }
});

it.each([
  { bytes: 4, type: "text/html" }, { bytes: 0, type: "image/png" }, { bytes: 256 * 1024 + 1, type: "image/png" },
])("rejects non-PNG, empty or oversized responses: %j", async ({ bytes, type }) => {
  vi.mocked(fetch).mockResolvedValue(new Response(new Uint8Array(bytes), { headers: { "content-type": type } }));
  await expect(fetchStudyPresentationPng(7, 3, new AbortController().signal)).rejects.toThrow();
});

it("fails closed on invalid currentPresentation in the list instead of enabling a guessed PNG", async () => {
  const currentPresentation = { ...presentation, snapshot: { ...snapshot, revision: -1 } } as CurrentStudyPresentation;
  vi.mocked(fetch).mockResolvedValue(json({ studies: [{ id: 7, status: "completed", currentPresentation }] }));
  await expect(listOptimizationStudies("spike", new AbortController().signal)).rejects.toThrow("Некорректный сохранённый PNG-снимок");
});
