import { afterEach, expect, it, vi } from "vitest";
import { ContinuousCleanupApiError, deleteContinuousHistory, previewContinuousCleanup } from "./continuous-cleanup-api";

const counts = { studies: 12, generations: 7, profiles: 3, checkpoints: 2, notificationJobs: 1, processesReset: 2 };
const retained = { runs: 9, batches: 4, qualityIntervals: 20, analyticsResults: 4, liveSetups: 19, configuredProcesses: 2, configuredSlots: 6 };
const preview = { eligible: true, blockers: [], fingerprint: "a".repeat(32), counts, retained };
afterEach(() => { vi.restoreAllMocks(); vi.unstubAllGlobals(); });

it("reads an exact authenticated owner-wide preview and confirms its opaque fingerprint once", async () => {
  const fetch = vi.fn((_path: string, init?: RequestInit) => Promise.resolve(new Response(JSON.stringify(init?.method === "POST" ? { deleted: counts, retained } : preview))));
  vi.stubGlobal("fetch", fetch);
  const signal = new AbortController().signal;
  expect(await previewContinuousCleanup(signal)).toEqual(preview);
  expect(await deleteContinuousHistory(preview.fingerprint, signal)).toEqual({ deleted: counts, retained });
  expect(fetch.mock.calls.map(([path]) => path)).toEqual(["/v1/continuous-studies/cleanup", "/v1/continuous-studies/cleanup"]);
  const post = fetch.mock.calls[1]?.[1];
  expect(post).toMatchObject({ method: "POST", signal, credentials: "same-origin", body: JSON.stringify({ confirm: true, fingerprint: preview.fingerprint }) });
  expect(new Headers(post?.headers).get("x-platform-request")).toBe("1");
  expect(new Headers(post?.headers).get("content-type")).toBe("application/json");
});
it.each([
  { counts: {} }, { counts: { ...counts, studies: -1 } }, { counts: { ...counts, checkpoints: 0.5 } },
  { retained: { ...retained, runs: "9" } }, { retained: { ...retained, liveSetups: Number.MAX_SAFE_INTEGER + 1 } },
  { fingerprint: "" }, { fingerprint: "z".repeat(32) }, { eligible: "true" }, { blockers: [null] },
])("rejects malformed cleanup preview fields without defaults: %j", async (patch) => {
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response(JSON.stringify({ ...preview, ...patch })))));
  await expect(previewContinuousCleanup(new AbortController().signal)).rejects.toThrow("неподдерживаемый предварительный просмотр");
});
it.each(["continuous_cleanup_blocked", "continuous_cleanup_preview_stale"])("preserves the validated replacement preview on %s without retrying", async (error) => {
  const next = { ...preview, fingerprint: "b".repeat(32), eligible: error !== "continuous_cleanup_blocked", blockers: error === "continuous_cleanup_blocked" ? ["continuous_not_stopped"] : [] };
  const fetch = vi.fn(() => Promise.resolve(new Response(JSON.stringify({ error, preview: next }), { status: 409 })));
  vi.stubGlobal("fetch", fetch);
  await expect(deleteContinuousHistory(preview.fingerprint, new AbortController().signal)).rejects.toMatchObject({ status: 409, preview: next });
  expect(fetch).toHaveBeenCalledOnce();
});
it("does not retain an invalid conflict preview and does not transmit an invalid confirmation", async () => {
  const fetch = vi.fn(() => Promise.resolve(new Response(JSON.stringify({ error: "continuous_cleanup_preview_stale", preview: {} }), { status: 409 })));
  vi.stubGlobal("fetch", fetch);
  await expect(deleteContinuousHistory(preview.fingerprint, new AbortController().signal)).rejects.toMatchObject({ preview: undefined });
  await expect(deleteContinuousHistory("not-a-preview", new AbortController().signal)).rejects.toThrow("Обновите предварительный просмотр");
  expect(fetch).toHaveBeenCalledOnce();
});
it.each([
  { status: 401, message: "Сессия истекла" }, { status: 403, message: "Недостаточно прав" },
  { status: 404, message: "пока недоступна" }, { status: 400, message: "Подтверждение очистки отклонено" },
  { status: 503, message: "HTTP 503" },
])("reports cleanup failure %s without success", async ({ status, message }) => {
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("{}", { status }))));
  await expect(previewContinuousCleanup(new AbortController().signal)).rejects.toThrow(message);
});
it("requires a new preview rather than treating a malformed successful acknowledgement as deletion", async () => {
  vi.stubGlobal("fetch", vi.fn(() => Promise.resolve(new Response("{}"))));
  await expect(deleteContinuousHistory(preview.fingerprint, new AbortController().signal)).rejects.toBeInstanceOf(ContinuousCleanupApiError);
  await expect(deleteContinuousHistory(preview.fingerprint, new AbortController().signal)).rejects.toThrow("Результат очистки не подтверждён");
});
