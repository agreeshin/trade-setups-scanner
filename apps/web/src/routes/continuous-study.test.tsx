import { act, cleanup, fireEvent, render, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import type { ContinuousConfig, ContinuousProcess, ContinuousSlot } from "../lib/continuous-api";
import { applyInterfaceSettings, formatUnix } from "../lib/datetime-format";
import { AdaptiveProfiles, ContinuousControls, ContinuousResearch as ResearchSummary } from "./continuous-study";
import type { AdaptiveProfile } from "../lib/adaptive-profiles-api";
import type { AdaptiveProfilesController } from "./use-adaptive-profiles";
import type { ContinuousController } from "./use-continuous-study";
import { summarizeOptimizationStudy, type OptimizationSpec, type OptimizationStudy } from "../lib/optimization-api";
import { AlgorithmsPage } from "./algorithms-page";
import type { Algorithm } from "./platform-controls";

const algorithm: Algorithm = { id: "future", name: "Stable service name", externalName: "Мой алгоритм", version: "1", description: "Metadata", enabled: false,
  params: { futureThreshold: "2.5", opaqueFixed: "kept" }, fields: [{ key: "futureThreshold", label: "Новый порог", type: "decimal", min: 0, max: 100, unit: "%", optimization: { enabledByDefault: true, values: ["1.5", "2.5"] } }] };
const spec: OptimizationSpec = { algorithmId: "future", algorithmVersion: "1", algorithmParams: algorithm.params, algorithmSearch: {}, analyticsParams: { clusterEnabled: true, clusterGapMinutes: 480, timeShiftMinutes: 3, priceChangePct: 1 }, analyticsSearch: {}, instruments: [{ exchange: "bybit", category: "linear", symbol: "OLDUSDT" }], fromUnix: 1793491200, toUnix: 1794096000, direction: "short", setupMaturity: 0, horizonMinutes: 10080, tpLevels: [4], slLevels: [2], minClosedPerCell: 1, seed: 42, maxTrials: 256, maxSeconds: 3600 };
function study(id: number, origin: "manual" | "continuous", status: OptimizationStudy["status"]): OptimizationStudy {
  return { id, origin, status, spec, progress: { phase: "acquiring", evaluatedCandidates: 0, scorableCandidates: 0, maxTrials: 256, elapsedSeconds: 1, bestScore: null }, result: null, error: null, createdAt: "2026-11-12T12:00:00Z", updatedAt: "2026-11-12T12:00:00Z", events: [] };
}
let process: ContinuousProcess | null;
let studies: OptimizationStudy[];
let unavailable: boolean;
let draftStatus: number;
let drafts: Record<string, unknown>;
let renameStatus: number;
let permission: boolean | null;
let standaloneProfiles: AdaptiveProfile[] | null;
function asProfile(slot: ContinuousSlot): AdaptiveProfile {
  return { ...slot, algorithmId: "future", controlRevision: 1, acceptedGenerationId: slot.finalizedGenerationId ?? slot.pendingGenerationId ?? slot.activeGenerationId ?? 1,
    desiredRunning: slot.status !== "stopped", activeStatus: slot.activeStatus ?? (slot.activeGenerationId === null ? "inactive" : "active"), pendingGenerationId: slot.pendingGenerationId ?? null,
    direction: "short", adaptiveEnabled: true, blockingReason: null, error: null, pendingAttempts: 0, attemptLimit: 3, initializationStatus: "active", inResearchQueue: true };
}
function profileFixture(controller: ContinuousController): AdaptiveProfilesController {
  return { page: { profiles: controller.process?.slots.filter((slot) => slot.studyId !== null || slot.activeGenerationId !== null || slot.pendingGenerationId != null).map((slot) => ({ ...asProfile(slot), activeStatus: slot.activeStatus, pendingGenerationId: slot.pendingGenerationId })) ?? [], total: controller.process?.slots.length ?? 0, offset: 0, limit: 50,
    limits: { totalProfiles: 50, runningProfiles: 30 }, blockedHandoffs: [] }, permission: true, available: controller.available, loading: false, error: null, rowErrors: {}, busy: {}, permissionBusy: false,
    offset: 0, setOffset: vi.fn(), refresh: vi.fn(), command: vi.fn(), savePermission: vi.fn() } as AdaptiveProfilesController;
}
// Compose the same independent surfaces for the retained badge/popup regression matrix.
function ContinuousResearch({ algorithm, controller }: { algorithm: Algorithm; controller: ContinuousController }) {
  return <><AdaptiveProfiles algorithm={algorithm} controller={profileFixture(controller)} research={controller} /><ResearchSummary controller={controller}><ContinuousControls algorithm={algorithm} controller={controller} /></ResearchSummary></>;
}
const requests: { path: string; method: string; body: unknown }[] = [];
beforeEach(() => {
  process = null; studies = []; unavailable = false; draftStatus = 200; drafts = {}; renameStatus = 200; requests.length = 0; permission = null; standaloneProfiles = null;
  vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
    const method = init?.method ?? "GET";
    const body: unknown = typeof init?.body === "string" ? JSON.parse(init.body) : null;
    requests.push({ path, method, body });
    const json = (data: unknown, status = 200) => Promise.resolve(new Response(JSON.stringify(data), { status }));
    if (path === "/v1/algorithms") return json({ algorithms: [algorithm] });
    if (path === "/v1/instruments") return json({ instruments: [{ id: 1, exchange: "bybit", category: "linear", symbol: "OLDUSDT", status: "Delisted" }] });
    if (path === "/v1/settings/ui") return json({ value: { replay: { symbols: "OLDUSDT" } } });
    const draft = /^\/v1\/optimization-drafts\/(.+)$/.exec(path);
    if (draft) {
      const id = decodeURIComponent(draft[1]!);
      if (method === "PUT") {
        if (draftStatus !== 200) return json({ error: "draft_failed" }, draftStatus);
        drafts[id] = (body as { value: unknown }).value;
      }
      return json({ algorithmId: id, value: drafts[id] ?? null });
    }
    if (path.startsWith("/v1/continuous-studies?")) return unavailable ? json({}, 404) : json({ processes: process ? [process] : [] });
    if (path.startsWith("/v1/adaptive-profiles?")) {
      const profiles = standaloneProfiles ?? (process?.slots.filter((slot) => slot.activeGenerationId !== null || slot.pendingGenerationId != null).map(asProfile) ?? []);
      return json({ profiles, total: profiles.length, offset: 0, limit: 50, limits: { totalProfiles: 50, runningProfiles: 30 }, blockedHandoffs: [] });
    }
    if (path === "/v1/adaptive-live/future/permission") {
      if (method === "PUT") { permission = (body as { enabled: boolean }).enabled; if (process?.config) process = { ...process, config: { ...process.config, adaptiveEnabled: permission } }; }
      return json({ algorithmId: "future", enabled: permission ?? process?.config?.adaptiveEnabled ?? false });
    }
    if (path === "/v1/continuous-studies/future" && method === "PUT") {
      const config = body as ContinuousConfig;
      if (process?.config && JSON.stringify({ ...process.config, adaptiveEnabled: false }) === JSON.stringify({ ...config, adaptiveEnabled: false })) {
        process = { ...process, config: { ...process.config, adaptiveEnabled: config.adaptiveEnabled } };
        return json(process);
      }
      if (process && process.status !== "stopped") return json({ error: "continuous_process_active" }, 409);
      process = { ownerId: "admin", algorithmId: "future", status: "stopped", configRevision: (process?.configRevision ?? 0) + 1, config, activeStudyId: null, phase: null, error: null,
        slots: config.minSetupsPerDay.map((threshold, index) => ({ id: index + 1, minSetupsPerDay: threshold, status: "idle", activeGenerationId: null, studyId: null, meanExpectancy: null, selectedCell: null, updatedAt: "2026-11-12T12:00:00Z" })) };
      return json(process);
    }
    if (path.endsWith("/future/start") && process) { process = { ...process, status: "running", phase: "queued" }; return json(process, 202); }
    if (path.endsWith("/future/stop") && process) { process = { ...process, status: "stopping", phase: "cleanup" }; return json(process, 202); }
    if (path.endsWith("/future/recovery/retry") && process) { process = { ...process, status: "running", error: null }; return json(process, 202); }
    if (path.startsWith("/v1/optimization-studies?")) {
      const query = new URL(path, "http://localhost").searchParams;
      const limit = Number(query.get("limit")); const offset = Number(query.get("offset"));
      return json({ studies: studies.slice(offset, offset + limit).map(summarizeOptimizationStudy), total: studies.length, limit, offset });
    }
    const detail = /^\/v1\/optimization-studies\/(\d+)$/.exec(path);
    if (detail) return json(studies.find((item) => item.id === Number(detail[1])));
    if (path.startsWith("/v1/algorithms/") && method === "PUT") return json({}, renameStatus);
    return json({});
  }));
});
afterEach(() => { cleanup(); vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals(); applyInterfaceSettings({ timezone: "auto" }); });

async function configure() {
  const button = await screen.findByRole("button", { name: "Настроить непрерывное исследование" });
  await waitFor(() => expect(button).toBeEnabled());
  await userEvent.click(button);
  const dialog = within(await screen.findByRole("dialog", { name: "Непрерывное исследование · Мой алгоритм" }));
  await waitFor(() => expect(dialog.getByRole("button", { name: "Сохранить непрерывное исследование" })).toBeEnabled());
  return dialog;
}

it("saves and restores forecast settings and separates the forward gate from the mean threshold", async () => {
  render(<AlgorithmsPage />);
  let dialog = await configure();
  expect(dialog.getByLabelText("Перегиб на N дней")).toBeDisabled();
  fireEvent.click(dialog.getByRole("button", { name: "Прогноз по тренду" }));
  fireEvent.change(dialog.getByLabelText("Перегиб на N дней"), { target: { value: "30" } });
  expect(dialog.getByLabelText("Минимальное среднее матожидание, R")).toHaveValue(0.15);
  expect(dialog.getByLabelText("Минимальное среднее матожидание, R")).toBeDisabled();
  expect(dialog.getByLabelText("Минимальное среднее матожидание, R")).toHaveAccessibleDescription(/score > minForecastExpectancy/);
  expect(dialog.getByLabelText("Минимальный прогноз матрицы, R")).toHaveValue(0);
  fireEvent.click(dialog.getByRole("button", { name: "Сохранить непрерывное исследование" }));
  await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
  expect(process?.config?.study).toMatchObject({ trendMode: "forecast", trendBendDays: "30" });
  dialog = await configure();
  expect(dialog.getByRole("button", { name: "Прогноз по тренду" })).toHaveAttribute("aria-pressed", "true");
  expect(dialog.getByLabelText("Перегиб на N дней")).toHaveValue("30");
  expect(requests.some((request) => request.path.endsWith("/start"))).toBe(false);
});

it("reopens saved continuous blend settings without a draft and restores the non-default lambda and forward threshold", async () => {
  process = profileController({}, { status: "stopped" }).process;
  if (!process?.config) throw new Error("Expected configured process");
  process.config.study = { ...spec, trendMode: "blend", trendBendDays: "14", trendBlendLambda: 0.6, minForecastExpectancy: 0.25 };
  render(<AlgorithmsPage />);
  let dialog = await configure();
  expect(dialog.getByRole("button", { name: "Смесь среднего и прогноза" })).toHaveAttribute("aria-pressed", "true");
  expect(dialog.getByLabelText("Доля прогноза λ")).toHaveValue(0.6);
  expect(dialog.getByLabelText("Минимальный прогноз матрицы, R")).toHaveValue(0.25);
  expect(dialog.getByLabelText("Минимальное среднее матожидание, R")).toBeDisabled();
  expect(dialog.getByLabelText("Минимальное среднее матожидание, R")).toHaveAccessibleDescription(/score > minForecastExpectancy/);
  await userEvent.click(dialog.getByRole("button", { name: "Закрыть" }));
  drafts = {};
  dialog = await configure();
  expect(dialog.getByLabelText("Доля прогноза λ")).toHaveValue(0.6);
  expect(dialog.getByLabelText("Минимальный прогноз матрицы, R")).toHaveValue(0.25);
  await userEvent.click(dialog.getByRole("button", { name: "Сохранить непрерывное исследование" }));
  await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
  expect(process?.config?.study).toMatchObject({ trendMode: "blend", trendBlendLambda: 0.6, minForecastExpectancy: 0.25 });
  expect(requests.some((request) => request.path.endsWith("/start"))).toBe(false);
});

const closedBoundary = Date.parse("2026-11-12T12:00:00Z") / 1000;
const readinessSummary = { generationId: 7, totalInstruments: 822, readyInstruments: 815, waitingInstruments: 7,
  waitingReasons: [{ reason: "insufficient_history", count: 5 }, { reason: "waiting_tail", count: 2 }], otherWaitingInstruments: 0 };
function profileController(patch: Partial<ContinuousSlot> = {}, processPatch: Partial<ContinuousProcess> = {}, controllerPatch: Partial<ContinuousController> = {}): ContinuousController {
  const controller: ContinuousController = {
    process: { ownerId: "admin", algorithmId: "future", status: "running", configRevision: 1,
      config: { lookbackDays: 30, minSetupsPerDay: [4], minMeanExpectancy: 0.15, adaptiveEnabled: true, study: spec }, activeStudyId: null, phase: "queued", error: null,
      slots: [{ id: 1, minSetupsPerDay: 4, status: "warming", activeGenerationId: 7, studyId: 42,
        runningStudyId: null, finalizedGenerationId: 8, activeStudyId: 41, pendingStudyId: null, meanExpectancy: 0.21,
        selectedCell: { tpPct: 14, slPct: 4, rawR: 0.32, meanR: 0.21, robustScore: 0.01 }, updatedAt: "2026-11-12T12:00:00Z", ...patch }], ...processPatch },
    available: true, loading: false, error: null, busy: null, revision: 0,
    refresh: vi.fn(), save: vi.fn(() => Promise.resolve(true)), start: vi.fn(() => Promise.resolve(true)), stop: vi.fn(() => Promise.resolve(true)), retryRecovery: vi.fn(() => Promise.resolve(true)),
    ...controllerPatch,
  };
  return controller;
}
function profileView(patch: Partial<ContinuousSlot> = {}) {
  return <ContinuousResearch algorithm={algorithm} controller={profileController(patch)} />;
}

it("labels the accepted winner from saved forecast fields even when the edited configuration has another metric", () => {
  render(profileView({ activeStatus: "active", selectedCell: { tpPct: 14, slPct: 4, rawR: -0.3, meanR: -0.2,
    robustScore: 0.4, forecastR: 0.5, forecastMatrixR: 0.6 } }));
  expect(screen.getByText("Выбор по прогнозу: 0.50 R · score матрицы 0.60 R")).toBeInTheDocument();
  expect(screen.getByText(/TP 14% \/ SL 4% · -0.30 R/)).toBeInTheDocument();
});
function liveButton() {
  const row = within(screen.getByRole("table", { name: "Адаптивные профили · Мой алгоритм" })).getAllByRole("row")[1]!;
  return within(within(row).getAllByRole("cell")[2]!).getByRole("button");
}
function liveDetails() {
  fireEvent.keyDown(document, { key: "Escape" });
  fireEvent.mouseEnter(liveButton());
  return within(screen.getByRole("tooltip"));
}

it.each([100, 95, 94, 50, 49, 0])("keeps only the active status green at readiness %s/100 and reveals counts only on hover", (ready) => {
  render(profileView({ activeStatus: "active", liveReadinessSummary: { ...readinessSummary, totalInstruments: 100, readyInstruments: ready, waitingInstruments: 100 - ready, waitingReasons: [], otherWaitingInstruments: 100 - ready } }));
  const button = screen.getByRole("button", { name: /Действующее поколение #7/ });
  const badge = button.querySelector('[data-slot="badge"]');
  expect(button).toHaveClass("cursor-help", "focus-visible:ring-2");
  expect(button).not.toHaveClass("underline", "decoration-dotted", "underline-offset-4");
  expect(badge).toHaveClass("text-success");
  expect(badge).toHaveTextContent(/^Активно$/);
  expect(badge).not.toHaveTextContent("поколение");
  expect(within(button).getByText("Действующее поколение #7").closest('[data-slot="badge"]')).toBeNull();
  const summary = `Готовы ${ready} из 100 · ожидают ${100 - ready}`;
  expect(screen.queryByText(summary)).not.toBeInTheDocument();
  const popup = liveDetails();
  expect(popup.getByText(summary)).toBeVisible();
  expect(popup.queryByRole("alert")).not.toBeInTheDocument();
  expect(button).toHaveAccessibleDescription(expect.stringContaining(summary));
});

const liveStatusCases: { patch: Partial<ContinuousSlot>; label: string; color: "success" | "warning" | "danger"; problem?: boolean }[] = [
  ...["error", "failed"].map((status) => ({ patch: { status, activeStatus: "active" as const }, label: "Ошибка", color: "danger" as const, problem: true })),
  { patch: { activeStatus: "error" }, label: "Ошибка", color: "danger", problem: true },
  { patch: { activeStatus: "active", pendingStatus: "error" }, label: "Ошибка", color: "danger", problem: true },
  { patch: { activeStatus: "active", pendingError: { code: "initialization_error" } }, label: "Ошибка", color: "danger", problem: true },
  { patch: { activeStatus: "active", pendingError: false }, label: "Ошибка", color: "danger", problem: true },
  ...["active", "warming", "idle", "pending", "running", "researching", "queued", "research", "acquiring", "replay", "analytics", "cleanup", "inactive", "nonpass", "rejected", "no_winner", "stopped"].map((status) => ({
    patch: { status, activeStatus: "active" as const, pendingStatus: "warming" }, label: "Активно", color: "success" as const,
  })),
  { patch: { status: "stopped", activeGenerationId: null }, label: "Остановлен", color: "warning" },
  { patch: { status: "warming", activeGenerationId: null }, label: "Идёт инициализация", color: "warning" },
  { patch: { status: "active", activeStatus: "inactive", pendingStatus: "warming" }, label: "Идёт инициализация", color: "warning" },
  ...["pending", "ready", "waiting_resource"].map((pendingStatus) => ({ patch: { status: "active", activeGenerationId: null, pendingGenerationId: 8, pendingStatus }, label: "Ожидает", color: "warning" as const })),
  { patch: { status: "active", activeGenerationId: null, pendingGenerationId: 8 }, label: "Ожидает", color: "warning" },
  { patch: { status: "active", activeStatus: "inactive" }, label: "Неактивно", color: "warning" },
  ...["inactive", "nonpass", "rejected", "no_winner"].map((status) => ({ patch: { status, activeGenerationId: null }, label: "Неактивно", color: "warning" as const })),
  { patch: { status: "active" }, label: "Состояние live неизвестно (старый API)", color: "warning" },
  ...["idle", "pending", "running", "researching", "queued", "research", "acquiring", "replay", "analytics", "cleanup"].map((status) => ({ patch: { status, activeGenerationId: null }, label: "Ожидает", color: "warning" as const })),
  ...["active", "unrecognised"].map((status) => ({ patch: { status, activeGenerationId: null }, label: "Нет действующего поколения", color: "warning" as const })),
];
it.each(liveStatusCases)("renders only $label in a $color badge for $patch", ({ patch, label, color, problem }) => {
  render(profileView(patch));
  const popup = liveDetails();
  const badge = liveButton().querySelector('[data-slot="badge"]');
  expect(badge).toHaveTextContent(label);
  expect(badge?.textContent).toBe(label);
  expect(badge).toHaveClass(`text-${color}`);
  expect(popup.queryByRole("alert") !== null).toBe(problem === true);
});

it("explains an unconfirmed snapshot without recoloring the saved live status and retains icon-only actions", async () => {
  const controller = profileController({ activeStatus: "active", liveReadinessSummary: readinessSummary }, {}, { available: false });
  const view = render(<ContinuousResearch algorithm={algorithm} controller={controller} />);
  expect(screen.getByRole("button", { name: /Действующее поколение #7/ }).querySelector('[data-slot="badge"]')).toHaveClass("text-success");
  expect(liveDetails().getByRole("alert")).toHaveTextContent("Актуальность снимка не подтверждена");
  const labels = ["Обновить статус непрерывного процесса", "Настроить непрерывное исследование", "Остановить непрерывное исследование"] as const;
  for (const label of labels) {
    const button = screen.getByRole("button", { name: label });
    expect(button.querySelector("svg")).toHaveAttribute("aria-hidden", "true");
    expect(button).toHaveClass("size-9");
    expect(button).not.toHaveClass("size-14");
    expect(button.querySelector("svg")).toHaveClass("size-4");
    expect(button.textContent).toBe("");
  }
  expect(screen.getByRole("button", { name: labels[1] })).toBeDisabled();
  fireEvent.click(screen.getByRole("button", { name: labels[0] })); expect(controller.refresh).toHaveBeenCalledOnce();
  view.rerender(<ContinuousResearch algorithm={algorithm} controller={{ ...controller, available: true }} />);
  expect(liveDetails().queryByRole("alert")).not.toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: labels[2] }));
  expect(controller.stop).not.toHaveBeenCalled();
  fireEvent.click(within(screen.getByRole("dialog", { name: "Остановить непрерывное исследование?" })).getByRole("button", { name: "Остановить исследование" }));
  expect(controller.stop).toHaveBeenCalledOnce();
  await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
});

it("puts research commands and history cleanup only in Continuous Research, outside adaptive profiles", async () => {
  render(<AlgorithmsPage />);
  const controls = await screen.findByRole("region", { name: "Управление непрерывным исследованием" });
  const research = screen.getByRole("region", { name: "Непрерывные исследования" });
  expect(research).toContainElement(controls);
  for (const name of ["Обновить статус непрерывного процесса", "Настроить непрерывное исследование", "Запустить непрерывное исследование", "Удалить историю непрерывных исследований…"]) {
    expect(within(controls).getByRole("button", { name })).toBeInTheDocument();
  }
  const profiles = screen.getByRole("heading", { name: "Адаптивные профили" }).parentElement!.parentElement!;
  expect(profiles).not.toContainElement(controls);
  expect(within(profiles).queryByRole("button", { name: "Настроить непрерывное исследование" })).not.toBeInTheDocument();
  expect(within(controls).getByRole("button", { name: "Запустить непрерывное исследование" })).toHaveClass("size-9");
});

it("requires explicit research Stop confirmation, defaults focus to Cancel and restores trigger focus", async () => {
  const controller = profileController();
  render(<ContinuousResearch algorithm={algorithm} controller={controller} />);
  const trigger = screen.getByRole("button", { name: "Остановить непрерывное исследование" });
  await userEvent.click(trigger);
  let dialog = within(screen.getByRole("dialog", { name: "Остановить непрерывное исследование?" }));
  expect(dialog.getByText(/Профили, завершённая история и постоянный live сохраняются/)).toBeInTheDocument();
  expect(dialog.getByRole("button", { name: "Отмена" })).toHaveFocus();
  expect(controller.stop).not.toHaveBeenCalled();
  await userEvent.click(dialog.getByRole("button", { name: "Отмена" }));
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  expect(trigger).toHaveFocus();
  await userEvent.click(trigger);
  await userEvent.keyboard("{Escape}");
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  expect(trigger).toHaveFocus();
  expect(controller.stop).not.toHaveBeenCalled();
  await userEvent.click(trigger);
  dialog = within(screen.getByRole("dialog", { name: "Остановить непрерывное исследование?" }));
  await userEvent.click(dialog.getByRole("button", { name: "Остановить исследование" }));
  expect(controller.stop).toHaveBeenCalledOnce();
  await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
});

it.each([
  { available: false }, { busy: "save" as const }, { busy: "stop" as const },
  { process: null }, { process: profileController({}, { status: "stopped" }).process },
  { process: profileController({}, { status: "stopping" }).process },
])("rechecks command eligibility if the snapshot changes while research Stop confirmation is open: %j", (patch) => {
  const controller = profileController();
  const view = render(<ContinuousControls algorithm={algorithm} controller={controller} />);
  fireEvent.click(screen.getByRole("button", { name: "Остановить непрерывное исследование" }));
  view.rerender(<ContinuousControls algorithm={algorithm} controller={{ ...controller, ...patch }} />);
  const confirm = within(screen.getByRole("dialog")).getByRole("button", { name: "Остановить исследование" });
  expect(confirm).toBeDisabled();
  fireEvent.click(confirm);
  expect(controller.stop).not.toHaveBeenCalled();
});

it("serializes research Stop confirmation and keeps a failed command visible without auto-retry", async () => {
  let resolveStop: ((ok: boolean) => void) | undefined;
  const controller = profileController({}, {}, { stop: vi.fn(() => new Promise<boolean>((resolve) => { resolveStop = resolve; })) });
  const view = render(<ContinuousControls algorithm={algorithm} controller={controller} />);
  fireEvent.click(screen.getByRole("button", { name: "Остановить непрерывное исследование" }));
  const dialog = within(screen.getByRole("dialog"));
  const confirm = dialog.getByRole("button", { name: "Остановить исследование" });
  fireEvent.click(confirm); fireEvent.click(confirm);
  expect(controller.stop).toHaveBeenCalledOnce();
  expect(dialog.getByRole("button", { name: "Отмена" })).toBeDisabled();
  await userEvent.keyboard("{Escape}");
  expect(screen.getByRole("dialog")).toBeInTheDocument();
  await act(async () => { resolveStop?.(false); await Promise.resolve(); });
  view.rerender(<ContinuousControls algorithm={algorithm} controller={{ ...controller, error: "Ошибка остановки. Обновите статус." }} />);
  expect(dialog.getByRole("alert")).toHaveTextContent("Ошибка остановки");
  expect(dialog.getByRole("button", { name: "Остановить исследование" })).toBeEnabled();
  expect(controller.stop).toHaveBeenCalledOnce();
});

it.each(["running", "error"] as const)("explains a retained process error in state %s without repainting active slots", (status) => {
  render(<ContinuousResearch algorithm={algorithm} controller={profileController({ activeStatus: "active", liveReadinessSummary: readinessSummary }, { status, error: { code: "source_unavailable", message: "Связь с источником потеряна." } })} />);
  expect(screen.getByRole("button", { name: /Действующее поколение #7/ }).querySelector('[data-slot="badge"]')).toHaveClass("text-success");
  const alert = liveDetails().getByRole("alert");
  expect(alert).toHaveClass("bg-danger/10");
  expect(alert).toHaveTextContent(`Ошибка непрерывного процесса (status=${status}): source_unavailable: Связь с источником потеряна.`);
});
it("explains process error status even without a server error description", () => {
  render(<ContinuousResearch algorithm={algorithm} controller={profileController({ activeStatus: "active" }, { status: "error" })} />);
  expect(liveDetails().getByRole("alert")).toHaveTextContent("status=error");
  expect(within(screen.getByRole("tooltip")).getByRole("alert")).toHaveTextContent("Описание ошибки не передано.");
});
it("lists all simultaneous problems with bounded server descriptions and no opaque payload dump", () => {
  render(<ContinuousResearch algorithm={algorithm} controller={profileController({ status: "failed", activeStatus: "error", pendingStatus: "error",
    pendingError: { code: "init_failed", message: "Инициализация недоступна. " + "y".repeat(1000), debug: "private-init-payload" },
    liveReadinessSummary: { ...readinessSummary, generationId: 8, readyInstruments: -1 } },
  { status: "running", error: { code: "source_error", message: "Источник недоступен. " + "x".repeat(1000) } }, { available: false })} />);
  const alert = liveDetails().getByRole("alert");
  const items = within(alert).getAllByRole("listitem");
  expect(items).toHaveLength(7);
  for (const problem of ["status=failed", "activeStatus=error", "init_failed", "status=running", "source_error", "поколению #8", "действующего поколения #7", "Некорректные числа готовности", "Актуальность снимка не подтверждена"]) expect(alert).toHaveTextContent(problem);
  expect(items.every((item) => item.textContent.length < 500)).toBe(true);
  expect(alert).not.toHaveTextContent("private-init-payload");
  expect(alert).not.toHaveTextContent("x".repeat(301));
  expect(alert).not.toHaveTextContent("y".repeat(301));
});
it.each([
  { totalInstruments: 0, readyInstruments: 0, waitingInstruments: 0 }, { totalInstruments: Number.NaN }, { totalInstruments: 1.5 },
  { readyInstruments: Number.POSITIVE_INFINITY }, { readyInstruments: -1 }, { readyInstruments: 823 },
  { waitingInstruments: -1 }, { waitingInstruments: 6 }, { otherWaitingInstruments: -1 },
  { waitingReasons: [{ reason: "history_missing", count: -1 }] },
])("reports invalid readiness numbers without presenting them as trusted coverage or changing active color: %j", (patch) => {
  render(profileView({ activeStatus: "active", liveReadinessSummary: { ...readinessSummary, ...patch } }));
  const popup = liveDetails();
  expect(popup.getByRole("alert")).toHaveTextContent("Некорректные числа готовности");
  expect(popup.getByText("Готовность инструментов неизвестна.")).toBeInTheDocument();
  expect(popup.queryByText(/Готовы .* из/)).not.toBeInTheDocument();
  expect(screen.getByRole("button", { name: /Действующее поколение #7/ }).querySelector('[data-slot="badge"]')).toHaveClass("text-success");
});
it("reports a summary generation mismatch with both identities and leaves the main status active", () => {
  render(profileView({ activeStatus: "active", liveReadinessSummary: { ...readinessSummary, generationId: 8 } }));
  expect(liveDetails().getByRole("alert")).toHaveTextContent("Сводка готовности относится к поколению #8 вместо действующего поколения #7.");
  expect(screen.getByRole("button", { name: /Действующее поколение #7/ }).querySelector('[data-slot="badge"]')).toHaveClass("text-success");
});
it.each(["mean", "forecast", null, undefined] as const)("labels the saved matrix mode %s without inferring it from the edited configuration", (trendMode) => {
  const controller = profileController({ status: "active", activeStatus: "active", trendBlendLambda: null, ...(trendMode === undefined ? {} : { trendMode }) });
  if (!controller.process?.config) throw new Error("Expected configured process");
  controller.process.config.study = { ...spec, trendMode: trendMode === "forecast" ? "mean" : "forecast" };
  render(<ContinuousResearch algorithm={algorithm} controller={controller} />);
  const cell = screen.getByRole("cell", { name: /^0\.21 R/ });
  expect(cell.textContent).toBe(`0.21 R${trendMode ? ` · ${trendMode}` : ""}`);
  expect(cell.querySelector('[data-slot="badge"]')).toBeNull();
  if (trendMode) expect(within(cell).getByText(`· ${trendMode}`)).toHaveClass("text-muted-foreground");
  expect(liveDetails().queryByRole("alert")).not.toBeInTheDocument();
});
it.each([{ lambda: 0.7, shares: "30/70" }, { lambda: 0.6, shares: "40/60" }, { lambda: 0.29, shares: "71/29" }, { lambda: 0.95, shares: "5/95" }])("labels the saved blend shares $shares from lambda=$lambda, never the editable config", ({ lambda, shares }) => {
  const controller = profileController({ status: "active", activeStatus: "active", trendMode: "blend", trendBlendLambda: lambda });
  if (!controller.process?.config) throw new Error("Expected configured process");
  controller.process.config.study = { ...spec, trendMode: "blend", trendBlendLambda: 0.8 };
  render(<ContinuousResearch algorithm={algorithm} controller={controller} />);
  const cell = screen.getByRole("cell", { name: /^0\.21 R/ });
  expect(cell.textContent).toBe(`0.21 R · blend · ${shares}`);
  expect(within(cell).getByText(`· blend · ${shares}`)).toHaveClass("text-muted-foreground");
  expect(cell.querySelector('[data-slot="badge"]')).toBeNull();
  expect(liveDetails().queryByRole("alert")).not.toBeInTheDocument();
});
it("omits the mode when the saved matrix mean is missing", () => {
  render(profileView({ meanExpectancy: null, trendMode: "blend", trendBlendLambda: 0.6 }));
  const row = within(screen.getByRole("table", { name: "Адаптивные профили · Мой алгоритм" })).getAllByRole("row")[1]!;
  expect(within(row).getAllByRole("cell")[4]).toHaveTextContent(/^—$/);
});
const exhaustedRecovery: Partial<ContinuousProcess> = { status: "error", activeStudyId: 42,
  error: { code: "durable_resume_retry_exhausted", message: "Исчерпан бюджет восстановления." } };
const recoveryLabel = "Повторить восстановление исследования";

it("explicitly retries an exhausted same-study recovery via its distinct recovery POST, never Stop, maintenance Resume or a configuration write", async () => {
  process = profileController({ activeStatus: "active" }, exhaustedRecovery).process;
  const initialConfig = process?.config;
  render(<AlgorithmsPage />);
  const retry = await screen.findByRole("button", { name: recoveryLabel });
  await waitFor(() => expect(retry).toBeEnabled());
  expect(screen.getByRole("button", { name: "Остановить непрерывное исследование" })).toBeEnabled();
  fireEvent.mouseEnter(screen.getByRole("button", { name: "О повторе восстановления" }));
  expect(screen.getByText(/Повтор восстановления сохраняет тот же ID/)).toHaveTextContent("сохранённый вход и исходный абсолютный дедлайн, без нового окна");
  expect(screen.getByText(/Повтор восстановления сохраняет тот же ID/)).toHaveTextContent("возобновление допуска обслуживания его не сбрасывает");
  expect(requests.filter((request) => request.method !== "GET")).toHaveLength(0);
  await userEvent.click(retry);
  await waitFor(() => expect(requests.filter((request) => request.method !== "GET")).toEqual([
    { path: "/v1/continuous-studies/future/recovery/retry", method: "POST", body: null },
  ]));
  expect(process?.activeStudyId).toBe(42);
  expect(process?.slots[0]?.activeGenerationId).toBe(7);
  expect(process?.config).toEqual(initialConfig);
  await waitFor(() => expect(screen.queryByRole("button", { name: recoveryLabel })).not.toBeInTheDocument());
});

it.each([{ available: false }, { busy: "save" as const }, { busy: "start" as const }, { busy: "stop" as const }])("disables recovery retry for a stale snapshot or a busy controller: %j", (patch) => {
  const controller = profileController({}, exhaustedRecovery, patch);
  render(<ContinuousResearch algorithm={algorithm} controller={controller} />);
  const retry = screen.getByRole("button", { name: recoveryLabel });
  expect(retry).toBeDisabled(); fireEvent.click(retry);
  expect(controller.start).not.toHaveBeenCalled(); expect(controller.stop).not.toHaveBeenCalled(); expect(controller.save).not.toHaveBeenCalled();
});

it.each(["source_unavailable", "durable_resume_corrupt"])("leaves ordinary process error %s with its existing Stop and no additional Start", (code) => {
  render(<ContinuousResearch algorithm={algorithm} controller={profileController({}, { ...exhaustedRecovery, error: { code, message: "Ошибка источника или снимка." } })} />);
  expect(screen.queryByRole("button", { name: recoveryLabel })).not.toBeInTheDocument();
  expect(screen.queryByRole("button", { name: "Запустить непрерывное исследование" })).not.toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Остановить непрерывное исследование" })).toBeEnabled();
  expect(screen.getByText(/Технический сбой не означает непрохождение допуска/)).toHaveTextContent(code);
});

it.each([{ config: null }, { activeStudyId: null }, { status: "running" as const }, { error: null }])("does not offer exhausted recovery without the precise error state, configuration and retained study: %j", (patch) => {
  render(<ContinuousResearch algorithm={algorithm} controller={profileController({}, { ...exhaustedRecovery, ...patch })} />);
  expect(screen.queryByRole("button", { name: recoveryLabel })).not.toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Остановить непрерывное исследование" })).toBeEnabled();
});
it("shows partial initialization readiness for the full active-generation scope, not a claim that all instruments emit", () => {
  render(profileView({ activeStatus: "active", liveReadinessSummary: readinessSummary, pendingGenerationId: 8, pendingStatus: "warming" }));
  const live = liveDetails();
  expect(live.getByText("Готовы 815 из 822 · ожидают 7")).toBeInTheDocument();
  expect(live.getByText("Недостаточно истории: 5")).toBeInTheDocument();
  expect(live.getByText("Ожидание свежих закрытых минут: 2")).toBeInTheDocument();
  expect(live.getByText(/не подтверждение формирования или доставки сигналов/)).toBeInTheDocument();
  expect(live.getByText(/Ожидающие инструменты остаются в наборе/)).toBeInTheDocument();
  expect(screen.getByRole("cell", { name: /Кандидат #8/ })).not.toHaveTextContent("815");
  expect(screen.getAllByRole("row")).toHaveLength(2);
  expect(requests).toHaveLength(0);
});
it("renders at most three bounded reasons and aggregates all other waiting instruments", () => {
  const longReason = "unknown-" + "x".repeat(2000);
  render(profileView({ activeStatus: "active", liveReadinessSummary: { ...readinessSummary, totalInstruments: 12, readyInstruments: 1, waitingInstruments: 11,
    waitingReasons: [{ reason: longReason, count: 3 }, { reason: "waiting_tail", count: 1 }, { reason: "insufficient_history", count: 1 },
      ...Array.from({ length: 5 }, (_, index) => ({ reason: `other-${index}`, count: 1 }))], otherWaitingInstruments: 1 } }));
  liveDetails();
  const reasons = within(screen.getByRole("list", { name: "Причины ожидания инструментов" }));
  expect(reasons.getAllByRole("listitem")).toHaveLength(4);
  expect(reasons.getByText("Другие причины ожидания: 6")).toBeInTheDocument();
  expect(reasons.getByText(/Другая причина: unknown-/).textContent.length).toBeLessThan(160);
  expect(screen.queryByText(longReason)).not.toBeInTheDocument();
  expect(screen.queryByRole("dialog")).not.toBeInTheDocument();
  expect(requests).toHaveLength(0);
});
it.each([null, { ...readinessSummary, generationId: 8 }])("shows unknown readiness for absent or different-generation summaries: %j", (liveReadinessSummary) => {
  render(profileView({ activeStatus: "active", liveReadinessSummary, pendingGenerationId: 8, pendingStatus: "ready" }));
  liveDetails();
  expect(screen.getByText("Готовность инструментов неизвестна.")).toBeInTheDocument();
  expect(screen.queryByText(/Готовы \d+ из/)).not.toBeInTheDocument();
});
it.each(["toString", "__proto__"])("renders unfamiliar reason code %s as public text, never an inherited object property", (reason) => {
  render(profileView({ activeStatus: "active", liveReadinessSummary: { ...readinessSummary, waitingReasons: [{ reason, count: 7 }] } }));
  liveDetails();
  expect(screen.getByText(`Другая причина: ${reason}: 7`)).toBeInTheDocument();
});
it("never assumes legacy or candidate-only readiness belongs to an active generation", () => {
  const view = render(profileView({ activeStatus: "active" }));
  liveDetails();
  expect(screen.getByText("Готовность инструментов неизвестна.")).toBeInTheDocument();
  view.rerender(profileView({ activeGenerationId: null, pendingGenerationId: 7, pendingStatus: "ready", liveReadinessSummary: readinessSummary }));
  expect(screen.getByText("Готовность инструментов неизвестна.")).toBeInTheDocument();
  expect(screen.queryByText(/Готовы \d+ из/)).not.toBeInTheDocument();
  expect(screen.getByText("Адаптивные сигналы этого профиля не формируются.")).toBeInTheDocument();
});
it("does not label an explicitly inactive generation as having active initialization coverage", () => {
  render(profileView({ activeStatus: "inactive", liveReadinessSummary: readinessSummary }));
  liveDetails();
  expect(screen.getByText("Готовность инструментов неизвестна.")).toBeInTheDocument();
  expect(screen.queryByText(/Готовы \d+ из/)).not.toBeInTheDocument();
});
it("updates readiness as waiting instruments join the same generation without inferring unknown instruments ready", () => {
  const view = render(profileView({ activeStatus: "active", liveReadinessSummary: { ...readinessSummary, waitingReasons: [{ reason: "readiness_unknown", count: 7 }] } }));
  liveDetails();
  expect(screen.getByText("Готовность неизвестна: 7")).toBeInTheDocument();
  expect(screen.getByText("Готовы 815 из 822 · ожидают 7")).toBeInTheDocument();
  view.rerender(profileView({ activeStatus: "active", liveReadinessSummary: { ...readinessSummary, readyInstruments: 822, waitingInstruments: 0, waitingReasons: [] } }));
  expect(screen.getByText("Действующее поколение #7")).toBeInTheDocument();
  expect(screen.getByText("Готовы 822 из 822 · ожидают 0")).toBeInTheDocument();
  expect(screen.queryByRole("list", { name: "Причины ожидания инструментов" })).not.toBeInTheDocument();
  view.rerender(profileView({ activeGenerationId: 8, activeStatus: "active", liveReadinessSummary: readinessSummary }));
  expect(screen.getByText("Готовность инструментов неизвестна.")).toBeInTheDocument();
  expect(screen.queryByText(/Готовы \d+ из/)).not.toBeInTheDocument();
  expect(requests).toHaveLength(0);
});
it("does not turn a configured empty research slot into a standalone live profile", () => {
  render(profileView({ status: "stopped", activeGenerationId: null, activeStatus: "inactive", studyId: null, finalizedGenerationId: null, meanExpectancy: null, selectedCell: null,
    pendingGenerationId: null, pendingStatus: null, readinessFrontier: null, pendingError: null, liveWatermark: null }));
  expect(screen.getByRole("heading", { name: "Адаптивные профили" })).toBeInTheDocument();
  const table = within(screen.getByRole("table", { name: "Адаптивные профили · Мой алгоритм" }));
  expect(table.getAllByRole("row")).toHaveLength(1);
  expect(screen.getByText(/Принятых адаптивных профилей пока нет/)).toBeInTheDocument();
  expect(table.getByRole("columnheader", { name: "Изменение состояния профиля" })).toBeInTheDocument();
  expect(screen.queryByText(/не дата исследования/)).not.toBeInTheDocument();
  fireEvent.mouseEnter(screen.getByRole("button", { name: "О настроенных адаптивных профилях" }));
  expect(screen.getByRole("tooltip")).toHaveTextContent("не дата исследования");
  expect(requests).toHaveLength(0);
});
it("keeps finalized and incumbent provenance while new research starts, completes only its search, and fails downstream", () => {
  const saved = { finalizedGenerationId: 8, studyId: 42, activeStudyId: 41, activeStatus: "active" as const,
    pendingGenerationId: 8, pendingStudyId: 42, pendingStatus: "warming" };
  const view = render(<ContinuousResearch algorithm={algorithm} controller={profileController(saved, { activeStudyId: 43, phase: "acquiring" })} />);
  const research = () => within(screen.getByRole("cell", { name: /Сохранённое исследование #42/ }));
  const unchanged = () => {
    expect(research().getByText("Сохранённое исследование #42")).toBeInTheDocument();
    expect(research().getByText("Полностью завершённое поколение #8")).toBeInTheDocument();
    expect(screen.queryByText("Сохранённое исследование #43")).not.toBeInTheDocument();
    expect(liveDetails().getByText("Источник метрик и рекомендации: исследование #41 · поколение #7")).toBeInTheDocument();
    expect(screen.getByRole("cell", { name: /Кандидат #8/ })).toHaveTextContent("Исследование кандидата #42");
    expect(screen.getByText("SHORT · TP 14% / SL 4% · 0.32 R")).toBeInTheDocument();
  };
  expect(screen.getByRole("region", { name: "Непрерывные исследования" })).toHaveTextContent("Исследование #43"); unchanged();
  view.rerender(<ContinuousResearch algorithm={algorithm} controller={profileController(saved, { activeStudyId: 43, phase: "analytics" })} />);
  expect(screen.getByRole("region", { name: "Непрерывные исследования" })).toHaveTextContent("Исследование #43"); unchanged();
  view.rerender(<ContinuousResearch algorithm={algorithm} controller={profileController(saved, { status: "error", activeStudyId: null })} />);
  expect(screen.getByRole("region", { name: "Непрерывные исследования" })).not.toHaveTextContent("Исследование #43"); unchanged();
  expect(requests).toHaveLength(0);
});
it("never uses unfinalized or legacy research slots as a fallback for the independent profiles endpoint", async () => {
  process = profileController({ runningStudyId: 26, finalizedGenerationId: null, studyId: null,
    activeGenerationId: null, activeStudyId: null, pendingGenerationId: null, pendingStatus: null,
    meanExpectancy: null, selectedCell: null, status: "acquiring" }, { activeStudyId: 26 }).process;
  standaloneProfiles = [];
  render(<AlgorithmsPage />);
  await screen.findByText(/Принятых адаптивных профилей пока нет/);
  expect(screen.getByRole("region", { name: "Непрерывные исследования" })).toHaveTextContent("Исследование #26");
  expect(screen.queryByText("Сохранённое исследование #26")).not.toBeInTheDocument();
  process = profileController({ studyId: 26 }).process;
  await userEvent.click(screen.getByRole("button", { name: "Обновить статус непрерывного процесса" }));
  expect(screen.getByRole("table", { name: "Адаптивные профили · Мой алгоритм" })).not.toHaveTextContent("#26");
  expect(screen.queryByText("Сохранённое исследование #26")).not.toBeInTheDocument();
});
it("separates successful research, incumbent live and the warming candidate without relabelling its study as the incumbent", () => {
  render(profileView({ activeStatus: "active", pendingGenerationId: 8, pendingStatus: "warming", readinessFrontier: null, pendingError: null, liveWatermark: closedBoundary }));
  const table = within(screen.getByRole("table", { name: "Адаптивные профили · Мой алгоритм" }));
  const incumbent = table.getByRole("cell", { name: /Действующее поколение #7/ });
  const candidate = table.getByRole("cell", { name: /Кандидат #8/ });
  expect(table.getByText("Исследование принято")).toBeInTheDocument();
  expect(table.getByText("Сохранённое исследование #42")).toBeInTheDocument();
  expect(incumbent).not.toHaveTextContent("#8"); expect(incumbent).not.toHaveTextContent("#42");
  expect(candidate).toHaveTextContent("Инициализация live по текущим данным");
  expect(candidate).not.toHaveTextContent("Готов к переключению");
  fireEvent.mouseEnter(screen.getByRole("button", { name: "О настроенных адаптивных профилях" }));
  expect(screen.getByRole("tooltip")).toHaveTextContent("без повторной проверки исторического результата");
  expect(screen.queryByText("Адаптивные сигналы этого профиля не формируются.")).not.toBeInTheDocument();
  expect(table.getByText("SHORT · TP 14% / SL 4% · 0.32 R")).toBeInTheDocument();
  expect(requests).toHaveLength(0);
});
it("distinguishes ready-to-switch from active and shows exact CLOSED frontier/watermark without adding a minute", () => {
  applyInterfaceSettings({ timezone: "UTC" });
  const view = render(profileView({ activeStatus: "active", pendingGenerationId: 8, pendingStatus: "ready", readinessFrontier: closedBoundary, pendingError: null, liveWatermark: 0 }));
  const candidate = screen.getByRole("cell", { name: /Кандидат #8/ });
  expect(candidate).toHaveTextContent("Готов к переключению · ещё не действует");
  expect(candidate).toHaveTextContent(`Граница готовности (закрытие минуты): ${formatUnix(closedBoundary)} (UTC)`);
  expect(liveDetails().getByText(`Watermark переключения: ${formatUnix(0)} (UTC)`)).toBeInTheDocument();
  view.rerender(profileView({ status: "active", activeStatus: "active", activeGenerationId: 8, pendingGenerationId: null, pendingStatus: null, readinessFrontier: null, pendingError: null, liveWatermark: closedBoundary }));
  expect(liveDetails().getByText(`Watermark переключения: ${formatUnix(closedBoundary)} (UTC)`)).toBeInTheDocument();
  expect(screen.queryByText(/Действующее поколение #7/)).not.toBeInTheDocument();
  expect(screen.getByText("Нет кандидата на замену")).toBeInTheDocument();
});
it("keeps the incumbent during a candidate error and renders only bounded public error text", () => {
  render(profileView({ activeStatus: "active", pendingGenerationId: 8, pendingStatus: "error", readinessFrontier: null,
    pendingError: { code: "history_missing", message: "Недостаточно истории. " + "x".repeat(1000), debug: "do-not-render-raw-payload" }, liveWatermark: closedBoundary }));
  expect(screen.getByRole("cell", { name: /Действующее поколение #7/ })).toBeInTheDocument();
  expect(screen.getByRole("cell", { name: /Кандидат #8/ })).toHaveTextContent("Ошибка инициализации live");
  const error = screen.getByRole("alert");
  expect(error).toHaveTextContent("history_missing"); expect(error).toHaveTextContent("Недостаточно истории.");
  expect(error.textContent.length).toBeLessThan(420); expect(error).not.toHaveTextContent("do-not-render-raw-payload");
  expect(screen.getByText("SHORT · TP 14% / SL 4% · 0.32 R")).toBeInTheDocument();
  expect(screen.queryByText("Адаптивные сигналы этого профиля не формируются.")).not.toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Остановить непрерывное исследование" })).toBeEnabled();
  expect(requests).toHaveLength(0);
});
it("does not turn a first successful candidate into an active generation before promotion", () => {
  render(profileView({ activeGenerationId: null, activeStatus: "inactive", pendingGenerationId: 8, pendingStatus: "warming", readinessFrontier: null, pendingError: null, liveWatermark: null }));
  expect(screen.getByText("Нет действующего поколения")).toBeInTheDocument();
  expect(screen.getByRole("cell", { name: /Кандидат #8/ })).toHaveTextContent("Инициализация live по текущим данным");
  expect(screen.queryByText(/Действующее поколение #8/)).not.toBeInTheDocument();
  expect(screen.queryByText("SHORT · TP 14% / SL 4% · 0.32 R")).not.toBeInTheDocument();
});
it("keeps accepted research visible but explicitly shows no adaptive emission after the first live initialization fails", () => {
  render(profileView({ minSetupsPerDay: 1, studyId: 16, activeGenerationId: null, activeStatus: "inactive", pendingGenerationId: 8,
    pendingStatus: "error", readinessFrontier: null, pendingError: { code: "current_data_unavailable", message: "Недостаточно текущих закрытых свечей." }, liveWatermark: null }));
  expect(screen.getByText("Исследование принято")).toBeInTheDocument();
  expect(screen.getByText("Сохранённое исследование #16")).toBeInTheDocument();
  expect(liveDetails().getByText("Адаптивные сигналы этого профиля не формируются.")).toBeInTheDocument();
  expect(screen.getByRole("cell", { name: /Кандидат #8/ })).toHaveTextContent("Ошибка инициализации live");
  expect(within(screen.getByRole("cell", { name: /Кандидат #8/ })).getByRole("alert")).toHaveTextContent("current_data_unavailable");
  expect(screen.queryByText(/Действующее поколение #8/)).not.toBeInTheDocument();
  expect(screen.queryByText("SHORT · TP 14% / SL 4% · 0.32 R")).not.toBeInTheDocument();
  expect(requests).toHaveLength(0);
});
it("labels legacy and partially rolled-out readiness as unknown rather than ready", () => {
  const view = render(profileView({ status: "active" }));
  expect(screen.getByText("Принятое поколение #7")).toBeInTheDocument();
  expect(screen.getByText("Состояние live неизвестно (старый API)")).toBeInTheDocument();
  expect(screen.getByText("Статус инициализации live неизвестен (старый API)")).toBeInTheDocument();
  expect(screen.queryByText(/Действующее поколение #7/)).not.toBeInTheDocument();
  expect(screen.queryByText(/Готов к переключению/)).not.toBeInTheDocument();
  view.rerender(profileView({ pendingGenerationId: 8 }));
  expect(screen.getByRole("cell", { name: /Кандидат #8/ })).toHaveTextContent("Статус инициализации live неизвестен");
});
it("shows an unrecognised candidate wait state without treating it as ready or dropping the incumbent", () => {
  render(profileView({ activeStatus: "active", pendingGenerationId: 8, pendingStatus: "waiting_resource", pendingError: "Ожидание ресурсного допуска" }));
  expect(screen.getByRole("cell", { name: /Кандидат #8/ })).toHaveTextContent("waiting_resource");
  expect(screen.queryByText(/Готов к переключению/)).not.toBeInTheDocument();
  expect(screen.getByRole("cell", { name: /Действующее поколение #7/ })).toBeInTheDocument();
  expect(screen.getByRole("alert")).toHaveTextContent("Ожидание ресурсного допуска");
});

it("saves actual metadata controls, date-free list/gate config and locked maturity without starting work", async () => {
  render(<AlgorithmsPage />);
  const dialog = await configure();
  expect(dialog.queryByLabelText("Начало периода сигналов")).not.toBeInTheDocument();
  expect(dialog.queryByLabelText("Конец периода сигналов")).not.toBeInTheDocument();
  expect(dialog.getByLabelText("Зрелость (фиксирована)")).toBeDisabled();
  expect(dialog.getByLabelText("Зрелость (фиксирована)")).toHaveValue(0);
  expect(dialog.getByLabelText("Минимальное среднее матожидание, R")).toHaveValue(0.15);
  expect(dialog.getByLabelText("Baseline: Новый порог")).toHaveValue("2.5");
  fireEvent.change(dialog.getByLabelText("Последние N дней"), { target: { value: "10" } });
  fireEvent.change(dialog.getByLabelText("Минимум сетапов в день — список профилей"), { target: { value: "0, 0.6, 4" } });
  fireEvent.change(dialog.getByLabelText("Baseline: Новый порог"), { target: { value: "3.75" } });
  await userEvent.click(dialog.getByRole("button", { name: "Сохранить непрерывное исследование" }));
  await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
  const put = requests.find((request) => request.path === "/v1/continuous-studies/future" && request.method === "PUT")!;
  const config = put.body as ContinuousConfig;
  expect(config).toMatchObject({ lookbackDays: 10, minSetupsPerDay: [0, 0.6, 4], minMeanExpectancy: 0.15, adaptiveEnabled: false,
    study: { algorithmParams: { futureThreshold: "3.75", opaqueFixed: "kept" }, algorithmSearch: { futureThreshold: ["1.5", "2.5"] }, instruments: [{ symbol: "OLDUSDT" }] } });
  for (const key of ["fromUnix", "toUnix", "minSetupsPerDay", "setupMaturity"]) expect(config.study).not.toHaveProperty(key);
  expect(requests.filter((request) => request.method !== "GET" && !request.path.startsWith("/v1/optimization-drafts/"))).toHaveLength(1);
  expect(screen.getByRole("checkbox", { name: "Разрешить адаптивное live-сканирование" })).not.toBeChecked();
  expect(screen.getByLabelText("Включён для live-сканирования")).not.toBeChecked();
});

it("shares common draft fields in both directions while preserving mode-specific values", async () => {
  render(<AlgorithmsPage />);
  await userEvent.click(await screen.findByRole("button", { name: "Запустить новую оптимизацию Мой алгоритм" }));
  let manual = within(await screen.findByRole("dialog", { name: "Оптимизация · Мой алгоритм" }));
  const manualBaseline = await manual.findByLabelText("Baseline: Новый порог");
  fireEvent.change(manual.getByLabelText("Начало периода сигналов"), { target: { value: "2026-10-05T01:02" } });
  fireEvent.change(manual.getByLabelText("Направление сделки"), { target: { value: "long" } });
  fireEvent.change(manual.getByLabelText("Горизонт сделки, мин (фиксирован)"), { target: { value: "600" } });
  fireEvent.change(manualBaseline, { target: { value: "7.5" } });
  fireEvent.blur(manualBaseline);
  await waitFor(() => expect(requests.filter((request) => request.path === "/v1/optimization-drafts/future" && request.method === "PUT")).toHaveLength(1));
  await userEvent.click(manual.getByRole("button", { name: "Закрыть" }));

  const continuous = await configure();
  expect(continuous.getByLabelText("Направление сделки")).toHaveValue("long");
  expect(continuous.getByLabelText("Горизонт сделки, мин (фиксирован)")).toHaveValue(600);
  expect(continuous.getByLabelText("Baseline: Новый порог")).toHaveValue("7.5");
  fireEvent.change(continuous.getByLabelText("Последние N дней"), { target: { value: "17" } });
  const tp = continuous.getByLabelText("Уровни TP, %");
  fireEvent.change(tp, { target: { value: "5, 9" } });
  fireEvent.blur(tp);
  await waitFor(() => expect(requests.filter((request) => request.path === "/v1/optimization-drafts/future" && request.method === "PUT")).toHaveLength(2));
  expect(drafts.future).toMatchObject({
    shared: { scope: { direction: "long", horizonMinutes: "600", tpLevels: "5, 9" } },
    manual: { from: "2026-10-05T01:02" },
    continuous: { lookbackDays: "17" },
  });
  await userEvent.click(continuous.getByRole("button", { name: "Закрыть" }));

  await userEvent.click(screen.getByRole("button", { name: "Запустить новую оптимизацию Мой алгоритм" }));
  manual = within(await screen.findByRole("dialog", { name: "Оптимизация · Мой алгоритм" }));
  expect(await manual.findByLabelText("Начало периода сигналов")).toHaveValue("2026-10-05T01:02");
  expect(manual.getByLabelText("Направление сделки")).toHaveValue("long");
  expect(manual.getByLabelText("Уровни TP, %")).toHaveValue("5, 9");
  expect(manual.queryByLabelText("Последние N дней")).not.toBeInTheDocument();
});

it("keeps adaptive permission independent and drains only through the common algorithm stop", async () => {
  render(<AlgorithmsPage />);
  const dialog = await configure();
  await userEvent.click(dialog.getByRole("button", { name: "Сохранить непрерывное исследование" }));
  await screen.findByRole("button", { name: "Запустить непрерывное исследование" });
  await userEvent.click(screen.getByRole("checkbox", { name: "Разрешить адаптивное live-сканирование" }));
  await userEvent.click(screen.getByRole("button", { name: "Сохранить разрешение адаптивного live" }));
  await screen.findByText("Разрешение адаптивного live сохранено.");
  expect(process?.config?.adaptiveEnabled).toBe(true);
  expect(requests.some((request) => request.path.endsWith("/start"))).toBe(false);
  await userEvent.click(screen.getByRole("button", { name: "Запустить непрерывное исследование" }));
  await screen.findByText("Непрерывное исследование включено");
  expect(screen.getByText("Адаптивный live разрешён · действующих профилей нет, ожидается инициализация.")).toBeInTheDocument();
  await userEvent.click(screen.getByRole("button", { name: "Остановить непрерывное исследование" }));
  expect(requests.filter((request) => request.path.endsWith("/stop"))).toHaveLength(0);
  await userEvent.click(within(screen.getByRole("dialog", { name: "Остановить непрерывное исследование?" })).getByRole("button", { name: "Остановить исследование" }));
  await screen.findByText("Останавливается");
  expect(screen.getByRole("button", { name: "Остановка запрошена · ожидаем drain" })).toBeDisabled();
  expect(requests.filter((request) => request.path.endsWith("/stop"))).toHaveLength(1);
  expect(requests.some((request) => request.path.includes("/cancel") || request.method === "DELETE")).toBe(false);
  expect(screen.getByLabelText("Включён для live-сканирования")).not.toBeChecked();
});

it("renames with an isolated partial PUT and never overwrites unsaved fixed parameters", async () => {
  render(<AlgorithmsPage />);
  await userEvent.click(await screen.findByRole("tab", { name: "Live-сканирование с постоянными параметрами" }));
  const input = await screen.findByRole("spinbutton", { name: "Новый порог (%)" });
  fireEvent.change(input, { target: { value: "8.5" } });
  fireEvent.change(screen.getByLabelText("Внешнее название алгоритма"), { target: { value: "  <b>Мой профиль</b>  " } });
  fireEvent.blur(screen.getByLabelText("Внешнее название алгоритма"));
  await screen.findByText("Название алгоритма сохранено.");
  expect(requests.find((request) => request.method === "PUT")?.body).toEqual({ externalName: "<b>Мой профиль</b>" });
  expect(screen.getByRole("tab", { name: "<b>Мой профиль</b>" })).toBeInTheDocument();
  expect(input).toHaveValue(8.5);
  await userEvent.click(screen.getByRole("button", { name: "Сохранить алгоритм" }));
  await screen.findByText("Настройки алгоритма сохранены.");
  expect(requests.filter((request) => request.method === "PUT")[1]?.body).toEqual({ params: { futureThreshold: "8.5", opaqueFixed: "kept" }, enabled: false });
});

it("keeps heading/name above Segmented and preserves fixed drafts across keyboard tab changes", async () => {
  render(<AlgorithmsPage />);
  const name = await screen.findByLabelText("Внешнее название алгоритма");
  const tabs = screen.getByRole("tablist", { name: "Режим live · Мой алгоритм" });
  expect(name.compareDocumentPosition(tabs) & Node.DOCUMENT_POSITION_FOLLOWING).not.toBe(0);
  expect(screen.queryByRole("button", { name: "Сохранить название" })).not.toBeInTheDocument();
  const flexible = within(tabs).getByRole("tab", { name: "Live-сканирование с гибкими параметрами" });
  const fixed = within(tabs).getByRole("tab", { name: "Live-сканирование с постоянными параметрами" });
  expect(flexible).toHaveAttribute("aria-selected", "true");
  expect(flexible.querySelector('[data-slot="segmented-status-dot"]')).toBeNull();
  expect(fixed.querySelector('[data-slot="segmented-status-dot"]')).toBeNull();
  fireEvent.keyDown(flexible, { key: "ArrowRight" });
  expect(fixed).toHaveFocus();
  expect(fixed).toHaveAttribute("tabindex", "0");
  const input = screen.getByRole("spinbutton", { name: "Новый порог (%)" });
  fireEvent.change(input, { target: { value: "8.5" } });
  fireEvent.click(screen.getByRole("checkbox", { name: "Включён для live-сканирования" }));
  expect(fixed.querySelector('[data-slot="segmented-status-dot"]')).toBeNull();
  fireEvent.keyDown(fixed, { key: "Home" });
  expect(flexible).toHaveFocus(); expect(input).not.toBeVisible();
  expect(document.getElementById(flexible.getAttribute("aria-controls")!)).toHaveAttribute("aria-labelledby", flexible.id);
  fireEvent.keyDown(flexible, { key: "End" });
  expect(screen.getByRole("spinbutton", { name: "Новый порог (%)" })).toBe(input);
  expect(input).toHaveValue(8.5);
  await userEvent.click(screen.getByRole("button", { name: "Сохранить алгоритм" }));
  await screen.findByText("Настройки алгоритма сохранены.");
  expect(fixed.querySelector('[data-slot="segmented-status-dot"]')).toHaveClass("bg-success");
  expect(fixed).toHaveAccessibleDescription("Live-сканирование включено");
});

it("maps the adaptive dot to saved permission, never research state, draft or readiness", async () => {
  process = profileController({}, { status: "running" }).process;
  if (!process?.config) throw new Error("Expected configured process");
  process = { ...process, config: { ...process.config, adaptiveEnabled: false } };
  render(<AlgorithmsPage />);
  const flexible = await screen.findByRole("tab", { name: "Live-сканирование с гибкими параметрами" });
  await screen.findByText("Непрерывное исследование включено");
  expect(flexible.querySelector('[data-slot="segmented-status-dot"]')).toBeNull();
  await userEvent.click(screen.getByRole("checkbox", { name: "Разрешить адаптивное live-сканирование" }));
  expect(flexible.querySelector('[data-slot="segmented-status-dot"]')).toBeNull();
  await userEvent.click(screen.getByRole("button", { name: "Сохранить разрешение адаптивного live" }));
  await screen.findByText("Разрешение адаптивного live сохранено.");
  expect(flexible.querySelector('[data-slot="segmented-status-dot"]')).toHaveClass("bg-success");
  expect(flexible).toHaveAccessibleDescription("Live-сканирование включено");
  expect(screen.getByRole("tab", { name: "Live-сканирование с постоянными параметрами" }).querySelector('[data-slot="segmented-status-dot"]')).toBeNull();
});

it("skips unchanged blur saves and surfaces rename failure without losing the retry draft", async () => {
  render(<AlgorithmsPage />);
  const input = await screen.findByLabelText("Внешнее название алгоритма");
  fireEvent.blur(input);
  expect(requests.every((request) => request.method === "GET")).toBe(true);
  renameStatus = 503;
  fireEvent.change(input, { target: { value: "Новое название" } });
  fireEvent.blur(input);
  expect(await screen.findByRole("alert")).toHaveTextContent("Не удалось сохранить: HTTP 503");
  expect(screen.getByRole("tab", { name: "Мой алгоритм" })).toBeInTheDocument();
  expect(input).toHaveValue("Новое название"); expect(input).toBeEnabled();
  renameStatus = 200;
  fireEvent.blur(input); fireEvent.blur(input);
  await screen.findByText("Название алгоритма сохранено.");
  expect(requests.filter((request) => request.method === "PUT")).toHaveLength(2);
  fireEvent.blur(input);
  expect(requests.filter((request) => request.method === "PUT")).toHaveLength(2);
});

it("shows unavailable rather than pretending the endpoint works and preserves fixed/manual controls", async () => {
  unavailable = true;
  render(<AlgorithmsPage />);
  await screen.findByText(/Непрерывные исследования пока недоступны на сервере/);
  expect(screen.getByRole("button", { name: "Настроить непрерывное исследование" })).toBeDisabled();
  expect(screen.getByRole("button", { name: "Запустить непрерывное исследование" })).toBeDisabled();
  expect(screen.getByText("Сохранить алгоритм")).toBeEnabled();
  expect(screen.getByRole("button", { name: "Запустить новую оптимизацию Мой алгоритм" })).toBeEnabled();
  expect(requests.every((request) => request.method === "GET")).toBe(true);
});

it("shows active and completed continuous history with structured origin, without a row/detail stop, and permits manual work", async () => {
  studies = [study(17, "continuous", "running"), study(16, "continuous", "completed")];
  render(<AlgorithmsPage />);
  const table = within(await screen.findByRole("table", { name: "Исследования · Мой алгоритм" }));
  await table.findByRole("button", { name: "Результат исследования 17" });
  expect(table.getAllByText("Непрерывное")).toHaveLength(2);
  expect(table.queryByRole("button", { name: "Остановить исследование 17" })).not.toBeInTheDocument();
  expect(table.getByRole("button", { name: "Удалить исследование 16" })).toBeInTheDocument();
  await userEvent.click(table.getByRole("button", { name: "Результат исследования 17" }));
  const detail = within(await screen.findByRole("dialog", { name: "Исследование #17 · Мой алгоритм" }));
  await detail.findByText("Непрерывное");
  expect(detail.queryByRole("button", { name: "Остановить исследование" })).not.toBeInTheDocument();
  await userEvent.click(detail.getByRole("button", { name: "Закрыть" }));
  await userEvent.click(screen.getByRole("button", { name: "Запустить новую оптимизацию Мой алгоритм" }));
  const manual = within(await screen.findByRole("dialog", { name: "Оптимизация · Мой алгоритм" }));
  await waitFor(() => expect(manual.getByRole("button", { name: "Начать исследование" })).toBeEnabled());
  expect(requests.some((request) => request.path.endsWith("/cancel"))).toBe(false);
});

it.each(["", "x".repeat(121), "bad\u0001name"])("rejects invalid external names without a write", async (name) => {
  render(<AlgorithmsPage />);
  fireEvent.change(await screen.findByLabelText("Внешнее название алгоритма"), { target: { value: name } });
  fireEvent.blur(screen.getByLabelText("Внешнее название алгоритма"));
  await screen.findByText("Название: от 1 до 120 символов, без управляющих символов.");
  expect(requests.every((request) => request.method === "GET")).toBe(true);
});
it("accepts 120 Unicode characters and Enter saves the name, not the fixed settings form", async () => {
  render(<AlgorithmsPage />);
  const input = await screen.findByLabelText("Внешнее название алгоритма");
  const name = "𐐷".repeat(120);
  input.focus();
  fireEvent.change(input, { target: { value: name } });
  fireEvent.keyDown(input, { key: "Enter" });
  await screen.findByText("Название алгоритма сохранено.");
  expect(requests.filter((request) => request.method !== "GET")).toEqual([{ path: "/v1/algorithms/future", method: "PUT", body: { externalName: name } }]);
});
it("observes without creating work and releases every polling timer on unmount", async () => {
  vi.useFakeTimers();
  const view = render(<AlgorithmsPage />);
  await act(async () => { await vi.advanceTimersByTimeAsync(0); });
  const reads = requests.filter((request) => request.path.startsWith("/v1/continuous-studies?")).length;
  await act(async () => { await vi.advanceTimersByTimeAsync(15000); });
  expect(requests.filter((request) => request.path.startsWith("/v1/continuous-studies?")).length).toBeGreaterThan(reads);
  expect(requests.every((request) => request.method === "GET")).toBe(true);
  view.unmount();
  const after = requests.length;
  await act(async () => { await vi.advanceTimersByTimeAsync(30000); });
  expect(requests).toHaveLength(after);
  expect(vi.getTimerCount()).toBe(0);
});
it("keeps an accepted profile visible through technical failure, allows common Stop and preserves completed history", async () => {
  render(<AlgorithmsPage />);
  const dialog = await configure();
  await userEvent.click(dialog.getByRole("button", { name: "Сохранить непрерывное исследование" }));
  await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
  if (!process?.config) throw new Error("Expected saved continuous config");
  process = { ...process, status: "error", config: { ...process.config, adaptiveEnabled: true }, phase: "analytics", error: { code: "source_unavailable", message: "Повторная попытка" },
    slots: [{ id: 1, minSetupsPerDay: 4, status: "error", activeGenerationId: 7, studyId: 42, meanExpectancy: 0.21, selectedCell: { tpPct: 14, slPct: 4, rawR: 0.32, meanR: 0.21, robustScore: 0.01 }, updatedAt: "2026-11-12T12:00:00Z" }] };
  studies = [study(42, "continuous", "completed")];
  await userEvent.click(screen.getByRole("button", { name: "Обновить статус непрерывного процесса" }));
  await userEvent.click(screen.getByRole("button", { name: "Обновить статус адаптивных профилей" }));
  await screen.findByText("Адаптивный live разрешён · принято профилей: 1.");
  liveDetails();
  expect(screen.getByText("Принятое поколение сохранено")).toBeInTheDocument();
  expect(screen.getByText("SHORT · TP 14% / SL 4% · 0.32 R")).toBeInTheDocument();
  await screen.findByRole("button", { name: "Результат исследования 42" });
  await userEvent.click(screen.getByRole("button", { name: "Остановить непрерывное исследование" }));
  await userEvent.click(within(screen.getByRole("dialog", { name: "Остановить непрерывное исследование?" })).getByRole("button", { name: "Остановить исследование" }));
  await screen.findByText("Останавливается");
  liveDetails();
  expect(screen.getByText("Принятое поколение сохранено")).toBeInTheDocument();
  expect(screen.getByText("SHORT · TP 14% / SL 4% · 0.32 R")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Результат исследования 42" })).toBeInTheDocument();
  expect(requests.some((request) => request.method === "DELETE")).toBe(false);
});
it("changes only live permission during running research without revision changes, profile loss or hidden Stop/start", async () => {
  render(<AlgorithmsPage />);
  const dialog = await configure();
  fireEvent.change(dialog.getByLabelText("Последние N дней"), { target: { value: "0.5" } });
  await userEvent.click(dialog.getByRole("button", { name: "Сохранить непрерывное исследование" }));
  await waitFor(() => expect(screen.queryByRole("dialog")).not.toBeInTheDocument());
  await userEvent.click(screen.getByRole("button", { name: "Запустить непрерывное исследование" }));
  await screen.findByText("Непрерывное исследование включено");
  if (!process?.config) throw new Error("Expected configured process");
  process = { ...process, activeStudyId: 17, phase: "acquiring", config: { ...process.config, adaptiveEnabled: true }, slots: [
    { id: 1, minSetupsPerDay: 4, status: "acquiring", activeGenerationId: 7, studyId: 17, meanExpectancy: 0.21, selectedCell: { tpPct: 14, slPct: 4, rawR: 0.32, meanR: 0.21, robustScore: 0.01 }, updatedAt: "2026-11-12T12:00:00Z" },
  ] };
  const before = process;
  await userEvent.click(screen.getByRole("button", { name: "Обновить статус непрерывного процесса" }));
  await userEvent.click(screen.getByRole("button", { name: "Обновить статус адаптивных профилей" }));
  await screen.findByText("Адаптивный live разрешён · принято профилей: 1.");
  await userEvent.click(screen.getByRole("checkbox", { name: "Разрешить адаптивное live-сканирование" }));
  await userEvent.click(screen.getByRole("button", { name: "Сохранить разрешение адаптивного live" }));
  await screen.findByText("Разрешение адаптивного live сохранено.");
  expect(process).toEqual({ ...before, config: { ...before.config, adaptiveEnabled: false } });
  expect(process?.configRevision).toBe(1);
  expect(requests.filter((request) => request.path === "/v1/adaptive-live/future/permission" && request.method === "PUT").at(-1)?.body).toEqual({ enabled: false });
  expect(requests.filter((request) => request.path === "/v1/continuous-studies/future" && request.method === "PUT")).toHaveLength(1);
  expect(requests.filter((request) => request.method === "POST").map((request) => request.path)).toEqual(["/v1/continuous-studies/future/start"]);
  expect(screen.getByText("Непрерывное исследование включено")).toBeInTheDocument();
  liveDetails();
  expect(screen.getByText("Принятое поколение сохранено")).toBeInTheDocument();
  expect(screen.getByLabelText("Включён для live-сканирования")).not.toBeChecked();
});
