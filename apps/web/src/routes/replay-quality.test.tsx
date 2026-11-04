import { useState } from "react";
import { act, cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, it, vi } from "vitest";
import { EmptyReplayResult, ReplayQualitySummary, type QualityPanelState, type ReplayQuality } from "./replay-quality";

afterEach(() => { cleanup(); vi.unstubAllGlobals(); });
const quality: ReplayQuality = {
  version: 1, state: "complete", requiredHistoryMinutes: 43200, requestedMinutes: 100, evaluatedMinutes: 100,
  evaluatedWithGapsMinutes: 4, skippedMinutes: 0, unassessedMinutes: 0, assessedThroughUnix: 6000,
  reasonCounts: { missing_candle: 0, insufficient_warmup: 0, recent_gap: 0, price_reference_missing: 0, gap_tolerance_exceeded: 0 },
};
function Harness({ value = quality, cacheHit = false }: { value?: ReplayQuality | null; cacheHit?: boolean }) {
  const [panel, setPanel] = useState<QualityPanelState>({ open: false, offset: 0 });
  return <><ReplayQualitySummary run={{ id: 7, quality: value, cacheHit, qualityComputedAt: cacheHit ? "2026-10-01T00:00:00Z" : null, qualitySourceRunId: cacheHit ? 2 : null }} panel={panel} onPanel={setPanel} revision={0} /><EmptyReplayResult quality={value} /></>;
}
it("distinguishes complete assessment with tolerated gaps from zero-gap input", () => {
  const fetcher = vi.fn(); vi.stubGlobal("fetch", fetcher);
  render(<Harness />);
  expect(screen.getByRole("heading", { name: /Полностью оценён по настроенным допускам/ })).toBeVisible();
  expect(screen.getByText("Из них с допустимыми пропусками окна").nextElementSibling).toHaveTextContent("4");
  expect(screen.getByText(/На 100 пригодных оценённых минутах сигналы не найдены/)).toBeVisible();
  expect(fetcher).not.toHaveBeenCalled();
});
it.each([null, undefined])("never turns unknown legacy quality %s into complete", (value) => {
  render(<><ReplayQualitySummary run={{ id: 7, cacheHit: false, ...(value === null ? { quality: null } : {}) }} panel={{ open: false, offset: 0 }} onPanel={() => undefined} revision={0} /><EmptyReplayResult quality={value} /></>);
  expect(screen.getByText("Качество не сохранено для этого запуска. Нулевое число сетапов не доказывает полноту анализа или пригодность данных.")).toBeVisible();
  expect(screen.getByText("Сетапов нет; качество и пригодность данных этого запуска неизвестны.")).toBeVisible();
});
it("shows no eligible data and warmup reason rather than successful empty analysis", async () => {
  render(<Harness value={{ ...quality, state: "no_eligible_data", evaluatedMinutes: 0, evaluatedWithGapsMinutes: 0, skippedMinutes: 100, reasonCounts: { ...quality.reasonCounts, insufficient_warmup: 100 } }} />);
  expect(screen.getByText("Нет пригодных данных для оценки. Это не успешный поиск с нулём сигналов.")).toBeVisible();
  await userEvent.click(screen.getByText("Причины пропуска минут (100)"));
  expect(screen.getByText("Недостаточно истории для прогрева").nextElementSibling).toHaveTextContent("100");
});
it("keeps unassessed cancellation/failure tail separate from skipped data", () => {
  render(<Harness value={{ ...quality, state: "in_progress", evaluatedMinutes: 20, skippedMinutes: 10, unassessedMinutes: 70, reasonCounts: { ...quality.reasonCounts, missing_candle: 10 } }} />);
  expect(screen.getByText("Рассмотрено минут").nextElementSibling).toHaveTextContent("30");
  expect(screen.getByText("Ещё не рассмотрено минут").nextElementSibling).toHaveTextContent("70");
  expect(screen.getByText(/Нерассмотренный остаток не является пропуском данных/)).toBeVisible();
});
it("retains original cache metadata and summary when source intervals are unavailable", async () => {
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(JSON.stringify({ available: false, intervals: [], offset: 0, limit: 50, total: 0 })))));
  render(<Harness cacheHit value={{ ...quality, state: "partial", evaluatedMinutes: 80, skippedMinutes: 20, reasonCounts: { ...quality.reasonCounts, recent_gap: 20 } }} />);
  expect(screen.getByText(/Из кеша.*Исходный запуск: #2/)).toBeVisible();
  expect(screen.getByText(/Последующая дозагрузка истории не обновляет эту сводку/)).toBeVisible();
  await userEvent.click(screen.getByRole("button", { name: "Показать интервалы качества" }));
  await screen.findByText(/исходный расчёт удалён/);
  expect(screen.getByRole("heading", { name: /Частично пригодные данные/ })).toBeVisible();
});
it("loads bounded intervals lazily and aborts stale requests when panel closes", async () => {
  const paths: string[] = []; const signals: (AbortSignal | null | undefined)[] = [];
  let resolveLate: ((response: Response) => void) | undefined;
  vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
    paths.push(path); signals.push(init?.signal);
    if (path.includes("offset=50")) return new Promise<Response>((resolve) => { resolveLate = resolve; });
    return Promise.resolve(new Response(JSON.stringify({ available: true, intervals: [{ fromUnix: 0, toUnix: 60, reason: "evaluated" }], offset: 0, limit: 50, total: 51 })));
  }));
  render(<Harness />);
  expect(paths).toHaveLength(0);
  await userEvent.click(screen.getByRole("button", { name: "Показать интервалы качества" }));
  await screen.findByText("Пригодно для оценки");
  expect(paths).toEqual(["/v1/replay-runs/7/quality-intervals?offset=0&limit=50"]);
  await userEvent.click(screen.getByRole("button", { name: "Следующие интервалы" }));
  await waitFor(() => expect(resolveLate).toBeDefined());
  expect(signals[0]?.aborted).toBe(true);
  await userEvent.click(screen.getByRole("button", { name: "Скрыть интервалы качества" }));
  expect(signals[1]?.aborted).toBe(true);
  await act(async () => { resolveLate?.(new Response(JSON.stringify({ available: true, intervals: [{ fromUnix: 60, toUnix: 120, reason: "recent_gap" }], offset: 50, limit: 50, total: 51 }))); await Promise.resolve(); });
  expect(screen.queryByRole("table")).not.toBeInTheDocument();
  await userEvent.click(screen.getByRole("button", { name: "Показать интервалы качества" }));
  expect(paths.at(-1)).toContain("offset=50&limit=50");
});
it("does not confuse a failed interval request with no gaps", async () => {
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("{}", { status: 502 }))));
  render(<Harness />);
  await userEvent.click(screen.getByRole("button", { name: "Показать интервалы качества" }));
  expect(await screen.findByRole("alert")).toHaveTextContent("Это не означает отсутствие пропусков");
});
