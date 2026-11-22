import { StrictMode } from "react";
import { act, cleanup, fireEvent, render, screen } from "@testing-library/react";
import { afterEach, beforeEach, expect, it, vi } from "vitest";
import { actualContinuousFixture } from "../../test/study-actual-fixture";
import { applyInterfaceSettings, DEFAULT_INTERFACE_SETTINGS } from "../lib/datetime-format";
import type { StudyActualContinuous } from "../lib/study-actual-continuous-api";
import { RouteActiveContext } from "./route-active";
import { StudyActualContinuousControls } from "./study-actual-continuous";

const label = "Включить непрерывный пересчёт факт матожидания";
const dateLabel = "Дата начала непрерывного пересчёта факт матожидания";
const toggle = () => screen.getByRole("switch", { name: label });
const date = () => screen.getByLabelText(dateLabel);
const json = (value: unknown, status = 200) => new Response(JSON.stringify(value), { status });
const requests: { path: string; method: string; body: unknown; signal: AbortSignal | null | undefined }[] = [];
let states: Record<string, StudyActualContinuous>;
let read: (algorithmId: string) => Response | Promise<Response>;
let write: (algorithmId: string, body: unknown) => Response | Promise<Response>;
const changed = vi.fn();
const settle = async (ms = 0) => { await act(async () => { await vi.advanceTimersByTimeAsync(ms); }); };
function surface(algorithmId = "spike", active = true, routeActive = true) {
  return <RouteActiveContext.Provider value={routeActive}><StudyActualContinuousControls algorithmId={algorithmId} active={active} onChanged={changed} /></RouteActiveContext.Provider>;
}
beforeEach(() => {
  vi.useFakeTimers(); requests.length = 0; changed.mockClear();
  applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "UTC" });
  vi.spyOn(document, "visibilityState", "get").mockReturnValue("visible");
  states = { spike: actualContinuousFixture(), drop: actualContinuousFixture("drop", { enabled: false, fromDate: "2026-11-01" }) };
  read = (algorithmId) => json(states[algorithmId]);
  write = (algorithmId, body) => {
    const update = body as { enabled: boolean; fromDate: string; timeZone: string; revision: number };
    states[algorithmId] = { ...states[algorithmId]!, ...update, revision: update.revision + 1 };
    return json(states[algorithmId]);
  };
  vi.stubGlobal("fetch", vi.fn((path: string, init?: RequestInit) => {
    const method = init?.method ?? "GET";
    const body: unknown = init?.body ? JSON.parse(init.body as string) : null;
    requests.push({ path, method, body, signal: init?.signal });
    const id = new URL(path, "http://localhost").searchParams.get("algorithmId")!;
    return Promise.resolve(method === "PUT" ? write(id, body) : read(id));
  }));
});
afterEach(() => { cleanup(); vi.useRealTimers(); vi.restoreAllMocks(); vi.unstubAllGlobals(); applyInterfaceSettings(DEFAULT_INTERFACE_SETTINGS); });

it("loads the server default, uses the project-formatted date, and never initializes settings with PUT/POST even in StrictMode", async () => {
  const view = render(<StrictMode>{surface()}</StrictMode>);
  expect(toggle()).toBeDisabled(); expect(screen.getByRole("status")).toHaveTextContent("Загрузка настройки");
  await settle();
  expect(toggle()).toBeEnabled(); expect(toggle()).toBeChecked(); expect(date()).toHaveValue("2026-11-22");
  expect(date().closest('[data-slot="date-control"]')).toHaveTextContent("22.11.2026");
  expect(date()).toHaveAccessibleDescription(label);
  const count = requests.length;
  act(() => { applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "Asia/Kathmandu", dateOrder: "ymd", dateSeparator: "/", yearDigits: 2 }); });
  expect(date().closest('[data-slot="date-control"]')).toHaveTextContent("26/11/22");
  expect(requests).toHaveLength(count); expect(requests.every((request) => request.method === "GET")).toBe(true);
  expect(requests.every((request) => request.path === "/v1/optimization-studies/actual/continuous?algorithmId=spike")).toBe(true);
  view.unmount(); await settle(60000); expect(vi.getTimerCount()).toBe(0);
});

it("saves the switch/date with the exact revision contract and restores saved state after reload", async () => {
  const view = render(surface()); await settle();
  fireEvent.click(toggle()); fireEvent.click(toggle());
  expect(toggle()).toBeDisabled(); await settle(); expect(toggle()).not.toBeChecked();
  expect(requests.filter((request) => request.method === "PUT")).toEqual([expect.objectContaining({ body: { enabled: false, fromDate: "2026-11-22", timeZone: "UTC", revision: 1 } })]);
  act(() => { applyInterfaceSettings({ ...DEFAULT_INTERFACE_SETTINGS, timezone: "Asia/Kathmandu" }); });
  fireEvent.change(date(), { target: { value: "2026-10-01" } }); await settle();
  expect(requests.at(-1)?.body).toEqual({ enabled: false, fromDate: "2026-10-01", timeZone: "Asia/Kathmandu", revision: 2 });
  expect(date()).toHaveValue("2026-10-01");
  const writes = requests.filter((request) => request.method === "PUT").length;
  view.unmount(); render(surface()); await settle();
  expect(toggle()).not.toBeChecked(); expect(date()).toHaveValue("2026-10-01");
  await settle(45000);
  expect(requests.filter((request) => request.method === "PUT")).toHaveLength(writes);
  expect(requests.some((request) => request.method === "POST")).toBe(false);
});

it("keeps unchanged polling quiet, invalidating history only on a changed queue/job/cursor or a saved command", async () => {
  render(surface()); await settle(); expect(changed).toHaveBeenCalledTimes(0);
  await settle(45000); expect(changed).toHaveBeenCalledTimes(0);
  states.spike = { ...states.spike!, queue: { manualQueued: 2, automaticQueued: 1, running: { studyId: 49, algorithmId: "spike", jobId: "job-49", status: "running", requestOrigin: "automatic" } } };
  await settle(15000); expect(changed).toHaveBeenCalledTimes(1);
  expect(screen.getByText("Выполняется пересчёт исследования #49 · автоматически")).toBeInTheDocument();
  expect(screen.getByText("В очереди: ручных 2, автоматических 1")).toBeInTheDocument();
  await settle(9000); expect(changed).toHaveBeenCalledTimes(1);
  states.spike = { ...states.spike, queue: { ...states.spike.queue, running: { ...states.spike.queue.running!, jobId: "job-49-next" } } };
  await settle(3000); expect(changed).toHaveBeenCalledTimes(2);
  fireEvent.click(toggle()); await settle(); expect(changed).toHaveBeenCalledTimes(3);
});

it("shows load failure and offers GET retry without an optimistic/default-success switch", async () => {
  read = () => json({}, 503);
  render(surface()); await settle();
  expect(toggle()).toBeDisabled(); expect(toggle()).not.toBeChecked(); expect(date()).toHaveValue("");
  expect(screen.getByRole("alert")).toHaveTextContent("HTTP 503");
  read = (id) => json(states[id]); fireEvent.click(screen.getByRole("button", { name: "Обновить настройку пересчёта" })); await settle();
  expect(toggle()).toBeChecked(); expect(toggle()).toBeEnabled(); expect(screen.queryByRole("alert")).not.toBeInTheDocument();
  expect(requests.every((request) => request.method === "GET")).toBe(true);
});

it("reconciles 409 with one GET, displays the conflict, and never resubmits PUT automatically", async () => {
  render(surface()); await settle();
  write = () => {
    states.spike = { ...states.spike!, revision: 9, fromDate: "2026-11-02" };
    return json({ error: "study_actual_configuration_changed", current: states.spike }, 409);
  };
  fireEvent.click(toggle()); await settle();
  expect(requests.map((request) => request.method)).toEqual(["GET", "PUT", "GET"]);
  expect(screen.getByRole("alert")).toHaveTextContent("другом клиенте");
  expect(toggle()).toBeChecked(); expect(date()).toHaveValue("2026-11-02");
  await settle(30000); expect(requests.filter((request) => request.method === "PUT")).toHaveLength(1);
});

it("reconciles a lost PUT acknowledgement with GET and does not issue an automatic retry", async () => {
  render(surface()); await settle();
  write = () => { states.spike = { ...states.spike!, enabled: false, revision: 2 }; throw new Error("lost ACK"); };
  fireEvent.click(toggle()); await settle();
  expect(toggle()).not.toBeChecked(); expect(screen.getByRole("alert")).toHaveTextContent("lost ACK");
  expect(requests.map((request) => request.method)).toEqual(["GET", "PUT", "GET"]);
});

it("fences stale algorithm GET/PUT responses and releases all owned requests on unmount", async () => {
  let finishRead: ((response: Response) => void) | undefined;
  read = (id) => id === "spike" ? new Promise<Response>((resolve) => { finishRead = resolve; }) : json(states[id]);
  const view = render(surface()); await settle(); const oldRead = requests[0]!;
  view.rerender(surface("drop")); await settle();
  expect(oldRead.signal?.aborted).toBe(true); expect(toggle()).not.toBeChecked(); expect(date()).toHaveValue("2026-11-01");
  await act(async () => { finishRead?.(json(states.spike)); await Promise.resolve(); }); await settle();
  expect(toggle()).not.toBeChecked(); expect(date()).toHaveValue("2026-11-01");
  let finishWrite: ((response: Response) => void) | undefined;
  write = () => new Promise<Response>((resolve) => { finishWrite = resolve; });
  fireEvent.click(toggle()); await settle(); const oldWrite = requests.at(-1)!;
  read = (id) => json(states[id]); view.rerender(surface("spike")); await settle();
  expect(oldWrite.signal?.aborted).toBe(true); expect(toggle()).toBeChecked(); expect(date()).toHaveValue("2026-11-22");
  await act(async () => { finishWrite?.(json(actualContinuousFixture("drop", { enabled: true }))); await Promise.resolve(); }); await settle();
  expect(date()).toHaveValue("2026-11-22");
  read = () => new Promise<Response>(() => undefined);
  await settle(15000);
  const pendingRead = requests.at(-1)!;
  const count = requests.length; view.unmount(); await settle(60000);
  expect(requests).toHaveLength(count); expect(pendingRead.signal?.aborted).toBe(true); expect(vi.getTimerCount()).toBe(0);
});

it.each(["route", "panel", "document"])("pauses GET polling at the %s boundary and resumes with exactly one read", async (boundary) => {
  const view = render(surface()); await settle(); const count = requests.length;
  if (boundary === "document") {
    vi.spyOn(document, "visibilityState", "get").mockReturnValue("hidden");
    act(() => { document.dispatchEvent(new Event("visibilitychange")); });
  } else view.rerender(surface("spike", boundary !== "panel", boundary !== "route"));
  await settle(60000); expect(requests).toHaveLength(count);
  if (boundary === "document") {
    vi.spyOn(document, "visibilityState", "get").mockReturnValue("visible");
    act(() => { document.dispatchEvent(new Event("visibilitychange")); });
  } else view.rerender(surface());
  await settle(); expect(requests).toHaveLength(count + 1);
});
