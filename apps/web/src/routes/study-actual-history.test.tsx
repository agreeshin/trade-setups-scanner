import { StrictMode } from "react";
import { act, cleanup, fireEvent, render, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { actualCountsFixture, studyActualFixture } from "../../test/study-actual-fixture";
import producerPagesJson from "../../test/study-actual-producer-pages.json?raw";
import type { OptimizationStudySummary } from "../lib/optimization-api";
import type { StudyActualSummary } from "../lib/study-actual-api";
import { OptimizationHistory } from "./optimization-history";
import type { OptimizationController } from "./use-optimization-studies";

function row(id: number, actual: StudyActualSummary | null = studyActualFixture(id)): OptimizationStudySummary {
  return { id, origin: "continuous", status: "completed", spec: { algorithmId: "spike", algorithmVersion: "1", algorithmParams: {}, algorithmSearch: {},
    analyticsParams: { clusterEnabled: true, clusterGapMinutes: 480, timeShiftMinutes: 0, priceChangePct: null }, analyticsSearch: {}, instruments: [],
    fromUnix: 1794000000, toUnix: 1794086400, direction: "short", setupMaturity: 0, horizonMinutes: 10080, tpLevels: [4], slLevels: [4],
    minClosedPerCell: 1, seed: 1, maxTrials: 10, maxSeconds: 60 }, progress: { phase: "done", evaluatedCandidates: 2, scorableCandidates: 2, maxTrials: 10, elapsedSeconds: 60, bestScore: 0.5 },
    bestScore: 0.5, forecastBest: { forecastR: 0.7, meanR: 0.1, tpPct: 4, slPct: 4 }, error: null, actual, createdAt: "2026-11-01T00:00:00Z", updatedAt: "2026-11-01T00:01:00Z" };
}
const controller: OptimizationController = { byAlgorithm: {}, latestByAlgorithm: {}, active: null, ready: true, error: null, creating: false, cancelling: null, deleting: null, revision: 0,
  start: vi.fn(() => Promise.resolve(true)), stop: vi.fn(() => Promise.resolve()), remove: vi.fn(() => Promise.resolve(true)), clearError: vi.fn(), refresh: vi.fn() };
let rows: OptimizationStudySummary[];
let post: (id: number) => Response | Promise<Response>;
let read: (id: number) => Response | Promise<Response>;
const requests: { path: string; method: string; body: unknown; signal?: AbortSignal | null }[] = [];
const json = (value: unknown, status = 200) => new Response(JSON.stringify(value), { status });
const button = (id = 49) => screen.getByRole("button", { name: `Пересчитать матожидание после исследования ${id}` });
const tableRow = (id = 49) => screen.getByRole("button", { name: `Результат исследования ${id}` }).closest("tr")!;
const posts = () => requests.filter((request) => request.method === "POST");
const settle = async (ms = 0) => { await act(async () => { await vi.advanceTimersByTimeAsync(ms); }); };

beforeEach(() => {
  vi.useFakeTimers(); requests.length = 0; rows = [row(49), row(48)];
  vi.spyOn(document, "visibilityState", "get").mockReturnValue("visible");
  post = (id) => {
    const next = studyActualFixture(id, { revision: 1, jobId: `job-${id}`, status: "queued", stage: "queued", observedThroughUnix: 1795201200 });
    rows = rows.map((item) => item.id === id ? { ...item, actual: next } : item); return json(next, 202);
  };
  read = (id) => json(rows.find((item) => item.id === id)?.actual);
  vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
    const method = init?.method ?? "GET";
    requests.push({ path, method, body: init?.body, ...(init?.signal ? { signal: init.signal } : {}) });
    if (path.startsWith("/v1/optimization-studies?")) {
      const params = new URL(path, "http://localhost").searchParams;
      const limit = Number(params.get("limit")); const offset = Number(params.get("offset"));
      return Promise.resolve(json({ studies: rows.slice(offset, offset + limit), total: rows.length, limit, offset }));
    }
    const actual = /^\/v1\/optimization-studies\/(\d+)\/actual(\/refresh)?$/.exec(path);
    if (actual) return Promise.resolve(actual[2] ? post(Number(actual[1])) : read(Number(actual[1])));
    return Promise.resolve(json({}, 404));
  }));
});
afterEach(() => { cleanup(); vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });
function surface(strict = false, pageSize = 5) {
  const node = <OptimizationHistory algorithmId="spike" algorithmName="Всплеск" controller={controller} pageSize={pageSize} pagination={false} onSelect={vi.fn()} onDelete={vi.fn()} />;
  return render(strict ? <StrictMode>{node}</StrictMode> : node);
}

it("mounts under StrictMode without any calculation or per-row discovery, keeping forecast distinct from actual", async () => {
  surface(true); await settle();
  expect(screen.getByRole("columnheader", { name: "Матожидание факт" })).toBeInTheDocument();
  expect(screen.getByRole("columnheader", { name: "Лучший прогноз матожидания" })).toBeInTheDocument();
  expect(within(tableRow()).getByText("0.70 R")).toBeInTheDocument();
  expect(within(tableRow()).getByText("Не рассчитано")).toBeInTheDocument();
  expect(posts()).toHaveLength(0); expect(requests.every((request) => request.path.includes("?"))).toBe(true);
});

it("blocks only the submitted study, spins its icon, deduplicates clicks, and allows another row in parallel", async () => {
  const pending = new Map<number, (response: Response) => void>();
  post = (id) => new Promise<Response>((resolve) => { pending.set(id, resolve); });
  const view = surface(); await settle();
  fireEvent.click(button()); fireEvent.click(button());
  expect(button()).toBeDisabled(); expect(button()).toHaveAttribute("aria-busy", "true");
  expect(button().querySelector("svg")).toHaveClass("motion-safe:animate-spin");
  expect(button(48)).toBeEnabled(); fireEvent.click(button(48));
  expect(posts()).toHaveLength(2); expect(posts().map((request) => request.body)).toEqual(["{}", "{}"]);
  view.unmount(); expect(posts().every((request) => request.signal?.aborted)).toBe(true);
  await act(async () => {
    for (const [id, resolve] of pending) resolve(json(studyActualFixture(id, { status: "queued", stage: "queued", revision: 1, jobId: `job-${id}` }), 202));
    await Promise.resolve();
  });
});

it("restores a persisted queued job after reload and observes completion using only compact page GET polling", async () => {
  rows[0] = row(49, studyActualFixture(49, { status: "queued", stage: "queued", revision: 2, jobId: "persisted" }));
  surface(); await settle(); expect(button()).toBeDisabled(); expect(within(tableRow()).getByRole("status")).toHaveTextContent("В очереди");
  cleanup(); surface(); await settle(); expect(button()).toBeDisabled();
  rows[0] = row(49, studyActualFixture(49, { status: "completed", stage: "completed", revision: 2, jobId: "persisted", counts: actualCountsFixture, meanR: -0.25 }));
  await settle(3000);
  expect(button()).toBeEnabled(); expect(within(tableRow()).getByText("-0.25R")).toHaveClass("text-danger");
  expect(within(tableRow()).getByText("123/176")).toHaveClass("text-ambient-violet");
  expect(posts()).toHaveLength(0);
  const count = requests.length; await settle(9000); expect(requests).toHaveLength(count);
});

it("uses closed/all counts, displays null as unavailable rather than zero, and exposes the exact count tooltip", async () => {
  rows[0] = row(49, studyActualFixture(49, { status: "completed", stage: "completed", counts: { total: 23, closed: 0, tp: 0, sl: 0, gateFailed: 20, unclosed: 3, noData: 0 } }));
  surface(); await settle();
  expect(within(tableRow()).getByText("0/23")).toBeInTheDocument(); expect(within(tableRow()).getByText("Нет закрытых сделок")).toBeInTheDocument();
  expect(within(tableRow()).queryByText("+0.00R")).not.toBeInTheDocument();
  fireEvent.focus(within(tableRow()).getByText("0/23"));
  expect(screen.getByRole("tooltip")).toHaveTextContent("Закрытые сделки / все сетапы");
  expect(screen.getByRole("tooltip")).toHaveTextContent("гейт не пройден: 20");
});

it.each([1.25, 0, -0.00001])("uses the unrounded sign of server mean %s", async (meanR) => {
  rows[0] = row(49, studyActualFixture(49, { status: "completed", stage: "completed", counts: actualCountsFixture, meanR }));
  surface(); await settle();
  const text = `${meanR >= 0 ? "+" : ""}${meanR.toFixed(2)}R`;
  expect(within(tableRow()).getByText(text)).toHaveClass(meanR === 0 ? "text-muted-foreground" : meanR > 0 ? "text-success" : "text-danger");
});

it("immediately removes previous fact display when a manual replacement starts", async () => {
  rows[0] = row(49, studyActualFixture(49, { status: "completed", stage: "completed", counts: actualCountsFixture, meanR: 1.25 }));
  surface(); await settle(); expect(within(tableRow()).getByText("+1.25R")).toBeInTheDocument();
  fireEvent.click(button()); expect(within(tableRow()).queryByText("+1.25R")).not.toBeInTheDocument();
  await settle(); expect(within(tableRow()).queryByText("123/176")).not.toBeInTheDocument(); expect(button()).toBeDisabled();
});

it("adopts a same-study busy job without resubmitting", async () => {
  const current = studyActualFixture(49, { status: "running", stage: "replaying", revision: 2, jobId: "other-tab" });
  post = () => json({ error: "study_actual_busy", actual: current }, 409);
  surface(); await settle(); fireEvent.click(button()); await settle();
  expect(button()).toBeDisabled(); expect(within(tableRow()).getByRole("status")).toHaveTextContent("Исторический прогон"); expect(posts()).toHaveLength(1);
});

it("reconciles uncertain POST acknowledgement with GET and never retries POST automatically", async () => {
  post = () => { throw new Error("connection lost after commit"); };
  read = () => json(studyActualFixture(49, { status: "queued", stage: "queued", revision: 3, jobId: "ack-lost" }));
  surface(); await settle(); fireEvent.click(button()); await settle();
  expect(button()).toBeDisabled(); expect(posts()).toHaveLength(1);
  expect(requests.some((request) => request.method === "GET" && request.path === "/v1/optimization-studies/49/actual")).toBe(true);
});

it("keeps an uncertain command blocked if reconciliation fails, with an explicit GET-only recovery action", async () => {
  post = () => { throw new Error("offline"); }; read = () => { throw new Error("offline"); };
  surface(); await settle(); fireEvent.click(button()); await settle();
  expect(button()).toBeDisabled(); expect(within(tableRow()).getByRole("alert")).toHaveTextContent("Новый запуск заблокирован");
  read = () => json(studyActualFixture(49, { status: "running", stage: "evaluating", revision: 1, jobId: "recovered" }));
  fireEvent.click(screen.getByRole("button", { name: "Проверить статус расчёта" })); await settle();
  expect(posts()).toHaveLength(1); expect(within(tableRow()).getByRole("status")).toHaveTextContent("Расчёт исходов");
});

it("shows terminal failure without reviving old mean or automatically retrying", async () => {
  rows[0] = row(49, studyActualFixture(49, { status: "failed", stage: "failed", error: { code: "market_unavailable", stage: "acquiring", message: "История недоступна" } }));
  surface(); await settle(); expect(within(tableRow()).getByRole("alert")).toHaveTextContent("market_unavailable: История недоступна");
  expect(button()).toBeEnabled(); await settle(9000); expect(posts()).toHaveLength(0);
});

it("does not infer legacy eligibility from completion status or invent missing execution start", async () => {
  rows[0] = row(49, studyActualFixture(49, { eligibility: { eligible: false, reason: "execution_start_unavailable" } }));
  rows[1] = row(48, null); surface(); await settle();
  expect(screen.getByRole("button", { name: /исследования 49 недоступен: execution_start_unavailable/ })).toBeDisabled();
  expect(screen.getByRole("button", { name: /исследования 48 недоступен/ })).toBeDisabled(); expect(posts()).toHaveLength(0);
});

it("cleans up pending page polling and requests on unmount", async () => {
  rows[0] = row(49, studyActualFixture(49, { status: "queued", stage: "queued", revision: 1, jobId: "server-owned" }));
  const view = surface(); await settle(); const count = requests.length; view.unmount(); await settle(15000);
  expect(requests).toHaveLength(count); expect(requests.every((request) => request.signal?.aborted)).toBe(true);
});

describe("real serialized compact-list producer parity", () => {
  type ProducerPhase = "idle" | "queued" | "running" | "completed" | "failed";
  // Captured cookie-authenticated native HTTP pages, not summarizeOptimizationStudy output.
  const pages = JSON.parse(producerPagesJson) as Record<ProducerPhase, { studies: { id: number; actual: StudyActualSummary }[] }>;
  let phase: ProducerPhase;
  beforeEach(() => {
    phase = "idle";
    vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
      const method = init?.method ?? "GET";
      requests.push({ path, method, body: init?.body, ...(init?.signal ? { signal: init.signal } : {}) });
      if (path.startsWith("/v1/optimization-studies?") && method === "GET") return Promise.resolve(json(pages[phase]));
      if (path === "/v1/optimization-studies/1/actual/refresh" && method === "POST") {
        phase = "queued"; return Promise.resolve(json(pages.queued.studies.find((study) => study.id === 1)!.actual, 202));
      }
      return Promise.resolve(json({ error: "unexpected request in captured-list parity" }, 404));
    }));
  });
  const legacy = () => screen.getByRole("button", { name: "Расчёт после исследования 2 недоступен: execution_start_unavailable" });
  const onlyCompactReads = () => expect(requests.filter((request) => request.method === "GET").every((request) => request.path.startsWith("/v1/optimization-studies?"))).toBe(true);

  it("makes the first manual click available from the real idle list alone and keeps the legacy row disabled", async () => {
    surface(false, 100); await settle();
    expect(button(1)).toBeEnabled(); expect(legacy()).toBeDisabled();
    expect(within(tableRow(1)).getByText("Не рассчитано")).toBeInTheDocument();
    expect(requests).toHaveLength(1); expect(posts()).toHaveLength(0); onlyCompactReads();
    fireEvent.click(legacy()); expect(posts()).toHaveLength(0);
    fireEvent.click(button(1)); fireEvent.click(button(1));
    expect(button(1)).toBeDisabled(); expect(posts()).toHaveLength(1); expect(posts()[0]?.body).toBe("{}");
    await settle();
    expect(within(tableRow(1)).getByRole("status")).toHaveTextContent("В очереди");
    expect(button(1)).toHaveAttribute("aria-busy", "true"); expect(legacy()).toBeDisabled();
    expect(posts()[0]?.path).toBe("/v1/optimization-studies/1/actual/refresh"); onlyCompactReads();
  });

  it.each(["queued", "running"] as const)("restores real %s state across reload and observes explicit completed counts using list polling only", async (pending) => {
    phase = pending;
    const view = surface(false, 100); await settle();
    expect(button(1)).toBeDisabled(); expect(button(1).querySelector("svg")).toHaveClass("motion-safe:animate-spin");
    const stage = pending === "queued" ? "В очереди" : "Расчёт исходов";
    expect(within(tableRow(1)).getByRole("status")).toHaveTextContent(stage);
    view.unmount(); surface(false, 100); await settle();
    expect(button(1)).toBeDisabled(); expect(within(tableRow(1)).getByRole("status")).toHaveTextContent(stage);
    phase = "completed"; await settle(3000);
    expect(button(1)).toBeEnabled(); expect(legacy()).toBeDisabled();
    expect(within(tableRow(1)).getByText("+0.00R")).toHaveClass("text-muted-foreground");
    const badge = within(tableRow(1)).getByText("2/2"); expect(badge).toHaveClass("text-ambient-violet");
    fireEvent.focus(badge);
    expect(screen.getByRole("tooltip")).toHaveTextContent("Закрытые сделки / все сетапы");
    expect(screen.getByRole("tooltip")).toHaveTextContent("TP: 1 · SL: 1 · гейт не пройден: 0 · не закрылись: 0 · нет данных: 0");
    expect(posts()).toHaveLength(0); onlyCompactReads();
    const count = requests.length; await settle(9000); expect(requests).toHaveLength(count);
  });

  it("renders the real failed revision and null counts without a fake zero, old badge or automatic retry", async () => {
    phase = "failed"; surface(false, 100); await settle();
    expect(within(tableRow(1)).getByRole("alert")).toHaveTextContent("fixture-failure: fixture-failure");
    expect(within(tableRow(1)).queryByText("+0.00R")).not.toBeInTheDocument();
    expect(within(tableRow(1)).queryByText("2/2")).not.toBeInTheDocument();
    expect(button(1)).toBeEnabled(); expect(legacy()).toBeDisabled();
    await settle(9000); expect(requests).toHaveLength(1); expect(posts()).toHaveLength(0); onlyCompactReads();
  });
});
