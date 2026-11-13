import { act, cleanup, fireEvent, render, renderHook, screen, waitFor, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, expect, it, vi } from "vitest";
import { ContinuousHistoryCleanup } from "./continuous-history-cleanup";
import { useContinuousStudy } from "./use-continuous-study";

const counts = { studies: 1, generations: 2, profiles: 1, checkpoints: 1, notificationJobs: 0, processesReset: 1 };
const retained = { runs: 9, batches: 4, qualityIntervals: 20, analyticsResults: 4, liveSetups: 19, configuredProcesses: 2, configuredSlots: 6 };
const preview = { eligible: true, blockers: [], fingerprint: "a".repeat(32), counts, retained };
const json = (value: unknown, status = 200) => new Response(JSON.stringify(value), { status });
afterEach(() => { cleanup(); vi.restoreAllMocks(); vi.unstubAllGlobals(); });
async function open() {
  await userEvent.click(screen.getByRole("button", { name: "Удалить историю непрерывных исследований…" }));
  return within(await screen.findByRole("dialog", { name: "Удалить всю историю непрерывных исследований?" }));
}
it.each([401, 404, 503])("fails closed when preview is unavailable (%s) and does not issue a mutation", async (status) => {
  const fetch = vi.fn(() => Promise.resolve(json({}, status)));
  vi.stubGlobal("fetch", fetch);
  render(<ContinuousHistoryCleanup onDeleted={vi.fn()} onRefreshStates={vi.fn()} />);
  const dialog = await open();
  await dialog.findByRole("alert");
  expect(dialog.getByRole("button", { name: "Удалить историю" })).toBeDisabled();
  expect(fetch).toHaveBeenCalledOnce();
});
it("does not infer ready from unknown blockers or an empty count set", async () => {
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(json({ ...preview, blockers: ["future_server_blocker"] }))));
  render(<ContinuousHistoryCleanup onDeleted={vi.fn()} onRefreshStates={vi.fn()} />);
  const dialog = await open();
  await dialog.findByText("Непрерывные исследования: 1");
  expect(dialog.getByRole("button", { name: "Удалить историю" })).toBeDisabled();
  expect(dialog.getByRole("alert")).toHaveTextContent("Сервер запретил очистку");
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(json({ ...preview, counts: Object.fromEntries(Object.keys(counts).map((key) => [key, 0])) }))));
  await userEvent.click(dialog.getByRole("button", { name: "Обновить предварительный просмотр" }));
  await dialog.findByText("Истории непрерывных исследований для очистки нет.");
  expect(dialog.getByRole("button", { name: "Удалить историю" })).toBeDisabled();
});
it("ignores a late preview after close/reopen and aborts requests on unmount", async () => {
  let resolveOld: ((response: Response) => void) | undefined;
  const signals: (AbortSignal | null | undefined)[] = [];
  vi.stubGlobal("fetch", vi.fn((_path: string, init?: RequestInit) => {
    signals.push(init?.signal);
    return signals.length === 1 ? new Promise<Response>((resolve) => { resolveOld = resolve; }) : Promise.resolve(json(preview));
  }));
  const view = render(<ContinuousHistoryCleanup onDeleted={vi.fn()} onRefreshStates={vi.fn()} />);
  let dialog = await open();
  expect(dialog.getByRole("button", { name: "Удалить историю" })).toBeDisabled();
  await userEvent.click(dialog.getByRole("button", { name: "Отмена" }));
  expect(signals[0]?.aborted).toBe(true);
  dialog = await open();
  await dialog.findByText("Непрерывные исследования: 1");
  await act(async () => { resolveOld?.(json({ ...preview, counts: { ...counts, studies: 999 } })); await Promise.resolve(); });
  expect(dialog.queryByText("Непрерывные исследования: 999")).not.toBeInTheDocument();
  view.unmount();
  expect(signals.at(-1)?.aborted).toBe(true);
});
it("requires a fresh preview after a lost acknowledgement and never auto-retries deletion", async () => {
  const onDeleted = vi.fn(); const onRefreshStates = vi.fn();
  const fetch = vi.fn((_path: string, init?: RequestInit) => init?.method === "POST" ? Promise.reject(new Error("network offline")) : Promise.resolve(json(preview)));
  vi.stubGlobal("fetch", fetch);
  render(<ContinuousHistoryCleanup onDeleted={onDeleted} onRefreshStates={onRefreshStates} />);
  const dialog = await open();
  await dialog.findByText("Непрерывные исследования: 1");
  await userEvent.click(dialog.getByRole("button", { name: "Удалить историю" }));
  expect(await dialog.findByRole("alert")).toHaveTextContent("network offline");
  expect(dialog.getByRole("button", { name: "Удалить историю" })).toBeDisabled();
  expect(onDeleted).not.toHaveBeenCalled(); expect(onRefreshStates).toHaveBeenCalledOnce();
  expect(fetch.mock.calls.filter(([, init]) => init?.method === "POST")).toHaveLength(1);
  await userEvent.click(dialog.getByRole("button", { name: "Обновить предварительный просмотр" }));
  await dialog.findByText("Непрерывные исследования: 1");
  expect(dialog.getByRole("button", { name: "Удалить историю" })).toBeEnabled();
});
it("serializes confirmation, prevents closing while deleting, and aborts mutation on unmount", async () => {
  let commandSignal: AbortSignal | null | undefined;
  let resolveCommand: ((response: Response) => void) | undefined;
  const onDeleted = vi.fn();
  const fetch = vi.fn((_path: string, init?: RequestInit) => {
    if (init?.method !== "POST") return Promise.resolve(json(preview));
    commandSignal = init.signal;
    return new Promise<Response>((resolve) => { resolveCommand = resolve; });
  });
  vi.stubGlobal("fetch", fetch);
  const view = render(<ContinuousHistoryCleanup onDeleted={onDeleted} onRefreshStates={vi.fn()} />);
  const dialog = await open();
  await dialog.findByText("Непрерывные исследования: 1");
  const confirm = dialog.getByRole("button", { name: "Удалить историю" });
  fireEvent.click(confirm); fireEvent.click(confirm);
  await waitFor(() => expect(dialog.getByRole("button", { name: "Отмена" })).toBeDisabled());
  await userEvent.keyboard("{Escape}");
  expect(screen.getByRole("dialog")).toBeInTheDocument();
  expect(fetch.mock.calls.filter(([, init]) => init?.method === "POST")).toHaveLength(1);
  view.unmount();
  expect(commandSignal?.aborted).toBe(true);
  await act(async () => { resolveCommand?.(json({ deleted: counts, retained })); await Promise.resolve(); });
  expect(onDeleted).not.toHaveBeenCalled();
});

it("fences a late pre-cleanup continuous-state read when the shared refresh revision changes", async () => {
  let resolveOld: ((response: Response) => void) | undefined;
  const signals: (AbortSignal | null | undefined)[] = [];
  const process = { algorithmId: "future", status: "stopped", configRevision: 2, config: null, activeStudyId: null, phase: null, error: null, slots: [] };
  vi.stubGlobal("fetch", vi.fn((_path: string, init?: RequestInit) => {
    signals.push(init?.signal);
    return signals.length === 1 ? new Promise<Response>((resolve) => { resolveOld = resolve; }) : Promise.resolve(json({ processes: [process] }));
  }));
  const view = renderHook(({ revision }) => useContinuousStudy("future", revision), { initialProps: { revision: 0 } });
  await waitFor(() => expect(resolveOld).toBeDefined());
  view.rerender({ revision: 1 });
  await waitFor(() => expect(view.result.current.process?.configRevision).toBe(2));
  expect(signals[0]?.aborted).toBe(true);
  await act(async () => { resolveOld?.(json({ processes: [{ ...process, status: "running", configRevision: 1, activeStudyId: 17 }] })); await Promise.resolve(); });
  expect(view.result.current.process).toEqual(process);
  view.unmount();
  expect(signals.at(-1)?.aborted).toBe(true);
});
