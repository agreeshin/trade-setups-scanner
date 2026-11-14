import { afterEach, expect, it, vi } from "vitest";
import { drainContinuousMaintenance, getContinuousMaintenance, resumeContinuousMaintenance, type ContinuousMaintenanceStatus } from "./continuous-maintenance-api";

const running: ContinuousMaintenanceStatus = { admissionPaused: false, state: "running", activeChains: 1, resumableChains: 1, updatedAt: null };
const draining: ContinuousMaintenanceStatus = { ...running, admissionPaused: true, state: "draining", updatedAt: "2026-11-14T12:00:00Z" };
const drained: ContinuousMaintenanceStatus = { ...draining, state: "drained", activeChains: 0, resumableChains: 0 };
afterEach(() => { vi.restoreAllMocks(); vi.unstubAllGlobals(); });

it("uses authenticated body-free GET/drain/resume routes without any Start or Stop", async () => {
  const fetch = vi.fn<typeof globalThis.fetch>().mockResolvedValueOnce(new Response(JSON.stringify(running)))
    .mockResolvedValueOnce(new Response(JSON.stringify(draining))).mockResolvedValueOnce(new Response(JSON.stringify(running)));
  vi.stubGlobal("fetch", fetch);
  const signal = new AbortController().signal;
  expect(await getContinuousMaintenance(signal)).toEqual(running);
  expect(await drainContinuousMaintenance(signal)).toEqual(draining);
  expect(await resumeContinuousMaintenance(signal)).toEqual(running);
  expect(fetch.mock.calls.map(([path]) => path)).toEqual(["/v1/continuous-studies/maintenance", "/v1/continuous-studies/maintenance/drain", "/v1/continuous-studies/maintenance/resume"]);
  for (const [index, [, init]] of vi.mocked(globalThis.fetch).mock.calls.entries()) {
    expect(init).toMatchObject({ method: index === 0 ? "GET" : "POST", signal, credentials: "same-origin" });
    expect(new Headers(init?.headers).get("x-platform-request")).toBe("1");
    expect(init?.body).toBeUndefined();
  }
});

it.each([running, draining, drained, { ...running, activeChains: 0, resumableChains: 0, updatedAt: "2026-11-14T15:00:00.123+03:00" }])("accepts canonical status without inventing recovery details: %j", async (status) => {
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(JSON.stringify(status)))));
  expect(await getContinuousMaintenance(new AbortController().signal)).toEqual(status);
});

it.each([
  null, true, {}, { ...running, admissionPaused: "false" }, { ...running, state: "paused" },
  { ...running, state: "drained" }, { ...draining, activeChains: 0 }, { ...drained, activeChains: 1 },
  { ...running, activeChains: -1 }, { ...running, activeChains: 1.5 }, { ...running, activeChains: "1" },
  { ...running, resumableChains: -1 }, { ...running, resumableChains: 0.5 }, { ...running, resumableChains: Number.MAX_SAFE_INTEGER + 1 },
  { ...running, updatedAt: undefined }, { ...running, updatedAt: 0 }, { ...running, updatedAt: "0" },
  { ...running, updatedAt: "2026-11-14" }, { ...running, updatedAt: "2026-11-14T12:00:00" }, { ...running, updatedAt: "2026-13-14T12:00:00Z" },
])("rejects malformed or contradictory maintenance status: %j", async (status) => {
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(JSON.stringify(status)))));
  await expect(getContinuousMaintenance(new AbortController().signal)).rejects.toThrow("неподдерживаемый статус");
});

it.each([
  [401, "Сессия истекла"], [403, "Недостаточно прав"], [404, "не поддерживается"], [405, "не поддерживается"],
  [409, "Состояние обслуживания изменилось"], [500, "HTTP 500"], [501, "не поддерживается"],
] as const)("reports HTTP %i without rendering raw error payloads", async (status, message) => {
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(JSON.stringify({ message: "PRIVATE_RAW_ERROR" }), { status }))));
  await expect(getContinuousMaintenance(new AbortController().signal)).rejects.toThrow(message);
  await expect(drainContinuousMaintenance(new AbortController().signal)).rejects.not.toThrow("PRIVATE_RAW_ERROR");
});

it("rejects a malformed success acknowledgement rather than assuming a command drained work", async () => {
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("not JSON"))));
  await expect(drainContinuousMaintenance(new AbortController().signal)).rejects.toThrow("неподдерживаемый статус");
});
