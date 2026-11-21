import { act, cleanup, fireEvent, render, renderHook, screen, within } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { adaptivePermission, listAdaptiveProfiles, type AdaptiveProfile, type ProfilePage } from "../lib/adaptive-profiles-api";
import { AdaptiveLive, AdaptiveProfiles } from "./continuous-study";
import { useAdaptiveProfiles } from "./use-adaptive-profiles";
import type { Algorithm } from "./platform-controls";

const algorithm: Algorithm = { id: "test", name: "Test", externalName: "Тест", version: "1", description: "Metadata", enabled: false, fields: [], params: {} };
function profile(id = 1): AdaptiveProfile {
  return { id, algorithmId: "test", status: "active", minSetupsPerDay: id, desiredRunning: true, controlRevision: 7, acceptedGenerationId: 22,
    activeGenerationId: 21, activeStatus: "active", pendingGenerationId: 22, pendingStatus: "warming", pendingError: null,
    readinessFrontier: null, liveWatermark: 1794484800, activeStudyId: 41, pendingStudyId: 42, studyId: 42,
    meanExpectancy: 0.21, selectedCell: { tpPct: 14, slPct: 4, rawR: 0.32, meanR: 0.21, robustScore: 0.29 }, trendMode: "forecast", direction: "long",
    adaptiveEnabled: true, blockingReason: null, error: null, pendingAttempts: 1, attemptLimit: 3, initializationStatus: "running", inResearchQueue: false,
    updatedAt: "2026-11-12T12:00:00Z", liveReadinessSummary: { generationId: 21, totalInstruments: 2, readyInstruments: 1, waitingInstruments: 1, waitingReasons: [{ reason: "waiting_tail", count: 1 }], otherWaitingInstruments: 0 } };
}
let rows: AdaptiveProfile[];
let enabled: boolean;
let status: number;
let commandStatus: number;
let permissionStatus: number;
let loseAck: boolean;
let slowList: ((response: Response) => void) | null;
let deferList: boolean;
let deferCommand: boolean;
const calls: { path: string; method: string; body: unknown; signal: AbortSignal | null | undefined }[] = [];
const json = (value: unknown, code = 200) => new Response(JSON.stringify(value), { status: code });
const tick = async (ms = 0) => { await act(async () => { await vi.advanceTimersByTimeAsync(ms); }); };
function page(offset = 0): ProfilePage { return { profiles: rows.slice(offset, offset + 50), total: rows.length, offset, limit: 50, limits: { totalProfiles: 50, runningProfiles: 30 }, blockedHandoffs: [] }; }
beforeEach(() => {
  vi.useFakeTimers(); rows = [profile()]; enabled = true; status = 200; commandStatus = 200; permissionStatus = 200; loseAck = false;
  deferList = false; slowList = null; deferCommand = false; calls.length = 0;
  vi.spyOn(document, "visibilityState", "get").mockReturnValue("visible");
  vi.stubGlobal("fetch", vi.fn(async (path: string, init?: RequestInit) => {
    const method = init?.method ?? "GET";
    const body = typeof init?.body === "string" ? JSON.parse(init.body) as Record<string, unknown> : null;
    calls.push({ path, method, body, signal: init?.signal });
    if (path.startsWith("/v1/adaptive-profiles?")) {
      if (deferList) return new Promise<Response>((resolve) => { slowList = resolve; });
      return json(page(Number(new URL(path, "http://localhost").searchParams.get("offset"))), status);
    }
    if (path === "/v1/adaptive-live/test/permission") {
      if (method === "PUT" && permissionStatus !== 200) return json({ error: "permission_save_failed" }, permissionStatus);
      if (method === "PUT") enabled = body?.enabled === true;
      return json({ algorithmId: "test", enabled });
    }
    const command = /^\/v1\/adaptive-profiles\/(\d+)\/(stop|play)$/.exec(path);
    if (command && body) {
      if (commandStatus !== 200) return json({ error: commandStatus === 429 ? "profile_running_limit" : "profile_revision_changed", limit: 30 }, commandStatus);
      const id = Number(command[1]); const play = command[2] === "play";
      if (loseAck) throw new Error("ACK lost");
      const receipt = { profileId: id, action: command[2], desiredRunning: play, controlRevision: Number(body.expectedRevision) + 1,
        acceptedGenerationId: body.expectedGenerationId, requestId: body.requestId, grantedAttempts: play ? 3 : 0, attemptLimit: play ? 6 : 3, status: play ? "warming" : "stopping" };
      if (deferCommand) return new Promise<Response>(() => { /* Abort is verified on unmount. */ });
      rows = rows.map((row) => row.id === id ? { ...row, desiredRunning: play, controlRevision: receipt.controlRevision, status: receipt.status, activeStatus: "inactive", pendingGenerationId: play ? row.acceptedGenerationId : null, pendingStatus: play ? "warming" : null, initializationStatus: play ? "retry" : row.initializationStatus } : row);
      return json(receipt);
    }
    throw new Error(`Unexpected endpoint ${path}`);
  }));
});
afterEach(() => { cleanup(); vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });
function Surface() {
  const controller = useAdaptiveProfiles("test");
  return <AdaptiveProfiles algorithm={algorithm} controller={controller} />;
}
function PermissionSurface() {
  const controller = useAdaptiveProfiles("test");
  return <><AdaptiveLive controller={controller} /><AdaptiveProfiles algorithm={algorithm} controller={controller} /></>;
}
const writes = () => calls.filter((call) => call.method !== "GET");
const reads = () => calls.filter((call) => call.path.startsWith("/v1/adaptive-profiles?"));

it.each([true, false])("keeps the permission switch draft local until explicit Save (saved=%s)", async (saved) => {
  enabled = saved;
  render(<PermissionSurface />);
  const control = screen.getByRole("switch", { name: "Разрешить адаптивное live-сканирование" });
  expect(control).toBeDisabled();
  await tick();
  expect(control).toBeEnabled();
  expect(control).toHaveAttribute("aria-checked", String(saved));
  const save = screen.getByRole("button", { name: "Сохранить разрешение адаптивного live" });
  expect(save).toBeDisabled();
  fireEvent.click(control);
  expect(control).toHaveAttribute("aria-checked", String(!saved));
  expect(save).toBeEnabled();
  expect(writes()).toHaveLength(0);
  expect(enabled).toBe(saved);
  fireEvent.click(save); await tick();
  expect(writes()).toEqual([expect.objectContaining({ path: "/v1/adaptive-live/test/permission", method: "PUT", body: { enabled: !saved } })]);
  expect(control).toHaveAttribute("aria-checked", String(!saved));
  expect(save).toBeDisabled();
  expect(screen.getByText("Разрешение адаптивного live сохранено.")).toBeInTheDocument();
});

it("retains an unsaved switch draft after Save fails without retrying or claiming success", async () => {
  render(<PermissionSurface />); await tick();
  permissionStatus = 503;
  const control = screen.getByRole("switch", { name: "Разрешить адаптивное live-сканирование" });
  const save = screen.getByRole("button", { name: "Сохранить разрешение адаптивного live" });
  fireEvent.click(control); fireEvent.click(save); await tick();
  expect(control).not.toBeChecked();
  expect(enabled).toBe(true);
  expect(writes()).toHaveLength(1);
  expect(save).toBeEnabled();
  expect(screen.getByRole("alert")).toHaveTextContent("HTTP 503");
  expect(screen.queryByText("Разрешение адаптивного live сохранено.")).not.toBeInTheDocument();
  permissionStatus = 200;
  fireEvent.click(save); await tick();
  expect(writes()).toHaveLength(2);
  expect(writes()[1]).toMatchObject({ path: "/v1/adaptive-live/test/permission", method: "PUT", body: { enabled: false } });
  expect(save).toBeDisabled();
});

it("loads retained out-of-queue profiles from the standalone API with their own direction and producing metric", async () => {
  render(<Surface />); await tick();
  expect(screen.getByText("Вне исследовательской очереди")).toBeInTheDocument();
  expect(screen.getByText("LONG · TP 14% / SL 4% · 0.32 R")).toBeInTheDocument();
  expect(screen.getByRole("cell", { name: /^0\.21 R\s*· forecast$/ })).toBeInTheDocument();
  expect(screen.getByRole("columnheader", { name: "Управление профилем" })).toBeInTheDocument();
  expect(writes()).toHaveLength(0);
  expect(calls.every((call) => !call.path.includes("continuous-studies"))).toBe(true);
  const source = new AbortController();
  expect(await adaptivePermission("test", source.signal, false)).toBe(false);
  expect(writes()).toEqual([expect.objectContaining({ path: "/v1/adaptive-live/test/permission", method: "PUT", body: { enabled: false } })]);
});

it("row Stop uses the profile revision and latest accepted generation, then rereads intent without inventing ACTIVE", async () => {
  render(<Surface />); await tick();
  fireEvent.click(screen.getByRole("button", { name: "Остановить адаптивный профиль 1" })); await tick();
  expect(writes()).toHaveLength(1);
  expect(writes()[0]).toMatchObject({ path: "/v1/adaptive-profiles/1/stop", method: "POST", body: { expectedRevision: 7, expectedGenerationId: 22 } });
  expect(String((writes()[0]!.body as { requestId: string }).requestId)).toMatch(/^[a-z0-9-]{16,128}$/i);
  expect(reads()).toHaveLength(2);
  expect(screen.getByText("Останавливается · ожидаем drain")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Запустить адаптивный профиль 1" })).toBeDisabled();
  expect(screen.getByText("LONG · TP 14% / SL 4% · 0.32 R")).toBeInTheDocument();
  rows[0] = { ...rows[0]!, status: "stopped" };
  fireEvent.click(screen.getByRole("button", { name: "Обновить статус адаптивных профилей" })); await tick();
  expect(screen.getByRole("button", { name: "Запустить адаптивный профиль 1" })).toBeEnabled();
});

it("exhausted Play grants via the profile endpoint, retries a lost ACK with the exact same idempotency key, and never resets research", async () => {
  rows = [{ ...profile(), desiredRunning: false, status: "stopped", activeStatus: "inactive", pendingGenerationId: null, pendingStatus: null, pendingAttempts: 3, initializationStatus: "exhausted" }];
  const view = renderHook(() => useAdaptiveProfiles("test")); await tick();
  loseAck = true;
  await act(async () => { await view.result.current.command(rows[0]!, "play"); }); await tick();
  expect(view.result.current.rowErrors[1]).toContain("ACK lost");
  expect(writes()).toHaveLength(1);
  loseAck = false;
  await act(async () => { await view.result.current.command(rows[0]!, "play"); }); await tick();
  expect(writes()).toHaveLength(2); expect(writes()[1]!.body).toEqual(writes()[0]!.body);
  expect(writes().every((call) => call.path === "/v1/adaptive-profiles/1/play")).toBe(true);
  expect(view.result.current.page?.profiles[0]).toMatchObject({ status: "warming", initializationStatus: "retry", desiredRunning: true });
});

it("keeps Stop available for an active incumbent while separately offering Play for an exhausted replacement", async () => {
  rows = [{ ...profile(), initializationStatus: "exhausted", pendingAttempts: 3, pendingStatus: "error" }];
  render(<Surface />); await tick();
  expect(screen.getByRole("button", { name: "Остановить адаптивный профиль 1" })).toBeEnabled();
  expect(screen.getByRole("button", { name: "Повторить запуск адаптивного профиля 1" })).toBeEnabled();
  fireEvent.click(screen.getByRole("button", { name: "Повторить запуск адаптивного профиля 1" })); await tick();
  expect(writes()).toHaveLength(1); expect(writes()[0]!.path).toBe("/v1/adaptive-profiles/1/play");
});

it("does not erase a permission save failure when the confirming GET succeeds", async () => {
  const view = renderHook(() => useAdaptiveProfiles("test")); await tick(); permissionStatus = 503;
  await act(async () => { expect(await view.result.current.savePermission(false)).toBe(false); }); await tick();
  expect(view.result.current.permission).toBe(true);
  expect(view.result.current.error).toContain("HTTP 503");
  expect(writes()).toHaveLength(1);
  permissionStatus = 200;
  await act(async () => { expect(await view.result.current.savePermission(false)).toBe(true); }); await tick();
  expect(view.result.current.error).toBeNull(); expect(view.result.current.permission).toBe(false);
});

it("rejects a late pre-command snapshot instead of reactivating a stopped row", async () => {
  const view = renderHook(() => useAdaptiveProfiles("test")); await tick();
  const before = page(); deferList = true; await tick(3000); const resolveOld = slowList;
  deferList = false;
  await act(async () => { await view.result.current.command(rows[0]!, "stop"); }); await tick();
  expect(view.result.current.page?.profiles[0]?.desiredRunning).toBe(false);
  await act(async () => { resolveOld?.(json(before)); await Promise.resolve(); }); await tick();
  expect(view.result.current.page?.profiles[0]).toMatchObject({ desiredRunning: false, controlRevision: 8, status: "stopping" });
  expect(writes()).toHaveLength(1);
});

it.each([409, 429])("retains the incumbent and displays row-scoped HTTP %s rejection without another mutation", async (code) => {
  commandStatus = code; render(<Surface />); await tick();
  fireEvent.click(screen.getByRole("button", { name: "Остановить адаптивный профиль 1" })); await tick();
  expect(screen.getByRole("alert")).toHaveTextContent(code === 429 ? "квота" : "Состояние профиля изменилось");
  expect(screen.getByText("Действующее поколение #21")).toBeInTheDocument();
  expect(writes()).toHaveLength(1);
});

it("keeps other profile controls available during a row command and blocks Play for independent global permission reasons", async () => {
  rows = [profile(), profile(2), { ...profile(3), desiredRunning: false, status: "stopped", blockingReason: "global_live_disabled" }];
  deferCommand = true; render(<Surface />); await tick();
  fireEvent.click(screen.getByRole("button", { name: "Остановить адаптивный профиль 1" })); await tick();
  expect(screen.getByRole("button", { name: "Остановить адаптивный профиль 1" })).toBeDisabled();
  expect(screen.getByRole("button", { name: "Остановить адаптивный профиль 2" })).toBeEnabled();
  expect(screen.getByRole("button", { name: "Запустить адаптивный профиль 3" })).toBeDisabled();
  expect(screen.getByText("Общий live выключен")).toBeInTheDocument();
  cleanup(); expect(calls.find((call) => call.method === "POST")?.signal?.aborted).toBe(true);
});

it("does not overlap slow polling, hides all reads on inactive panels/documents, and releases timers and requests", async () => {
  const view = renderHook(({ active }) => useAdaptiveProfiles("test", active), { initialProps: { active: true } }); await tick();
  deferList = true; await tick(3000); expect(reads()).toHaveLength(2);
  await tick(30000); expect(reads()).toHaveLength(2);
  await act(async () => { slowList?.(json(page())); await Promise.resolve(); }); await tick();
  view.rerender({ active: false }); const before = calls.length;
  await tick(60000); expect(calls).toHaveLength(before);
  deferList = false; view.rerender({ active: true }); await tick(); expect(reads()).toHaveLength(3);
  const visibility = vi.spyOn(document, "visibilityState", "get").mockReturnValue("hidden");
  await act(async () => { document.dispatchEvent(new Event("visibilitychange")); await Promise.resolve(); }); await tick(60000); expect(reads()).toHaveLength(3);
  visibility.mockReturnValue("visible"); await act(async () => { document.dispatchEvent(new Event("visibilitychange")); await Promise.resolve(); }); await tick(); expect(reads()).toHaveLength(4);
  view.unmount(); const after = calls.length; await tick(60000); expect(calls).toHaveLength(after); expect(vi.getTimerCount()).toBe(0);
});

it("fails closed for an unsupported server, without a process Stop fallback or an error-poll storm", async () => {
  status = 404; render(<Surface />); await tick();
  expect(screen.getByRole("alert")).toHaveTextContent("Общее управление исследованием не используется вместо Stop/Play профиля");
  await tick(60000); expect(reads()).toHaveLength(1); expect(writes()).toHaveLength(0);
});

it("retains the saved page but disables commands when the confirming GET fails", async () => {
  render(<Surface />); await tick(); status = 503;
  fireEvent.click(screen.getByRole("button", { name: "Обновить статус адаптивных профилей" })); await tick();
  expect(screen.getByText("Действующее поколение #21")).toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Остановить адаптивный профиль 1" })).toBeDisabled();
  expect(writes()).toHaveLength(0);
});

it("paginates bounded summaries and drops row errors when a profile leaves the page", async () => {
  rows = Array.from({ length: 51 }, (_, i) => profile(i + 1)); commandStatus = 429;
  render(<Surface />); await tick();
  const table = screen.getByRole("table"); expect(within(table).getAllByRole("row")).toHaveLength(51);
  fireEvent.click(screen.getByRole("button", { name: "Остановить адаптивный профиль 1" })); await tick();
  expect(screen.getByRole("alert")).toBeInTheDocument();
  fireEvent.click(screen.getByRole("button", { name: "Следующие профили" })); await tick();
  expect(within(table).getAllByRole("row")).toHaveLength(2); expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  expect(screen.getByRole("button", { name: "Остановить адаптивный профиль 51" })).toBeInTheDocument();
});

it("rejects malformed identities and wrong-owner/algorithm summaries before presenting controls", async () => {
  rows[0] = { ...profile(), algorithmId: "other" };
  await expect(listAdaptiveProfiles("test", 0, new AbortController().signal)).rejects.toThrow("неподдерживаемый список");
  expect(writes()).toHaveLength(0);
});
