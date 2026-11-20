import { apiFetch } from "./platform-api";

export type SetupTradeOutcomeStatus = "tp" | "sl" | "unclosed" | "no_data" | "gate_failed";

/** Integral live-setup data, not a browser cache or a parameter-matched analytics result. */
export interface SetupTradeOutcome {
  status: SetupTradeOutcomeStatus;
  entryTimeUnix: number | null;
  entryPrice: number | null;
  gateThreshold: number | null;
  tpPrice: number | null;
  slPrice: number | null;
  closeTimeUnix: number | null;
  closePrice: number | null;
  riskReturn: number | null;
  evaluatedAtUnix: number;
  observedThroughUnix: number;
  isFinal: boolean;
}

export interface SetupOutcomesRefreshPage {
  snapshotId: number;
  observedThroughUnix: number;
  processed: number;
  computed: number;
  updated: number;
  nextCursor: string | null;
}

const isRecord = (value: unknown): value is Record<string, unknown> =>
  value !== null && typeof value === "object" && !Array.isArray(value);
const isCount = (value: unknown): value is number =>
  typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
const isNullableNumber = (value: unknown): value is number | null =>
  value === null || (typeof value === "number" && Number.isFinite(value));
const isStatus = (value: unknown): value is SetupTradeOutcomeStatus =>
  value === "tp" || value === "sl" || value === "unclosed" || value === "no_data" || value === "gate_failed";

export function parseSetupTradeOutcome(value: unknown): SetupTradeOutcome | null {
  // Replay rows and an older feed may not expose the additive field yet.
  if (value === null || value === undefined) return null;
  if (!isRecord(value)
    || !isStatus(value.status)
    || !(value.entryTimeUnix === null || isCount(value.entryTimeUnix))
    || !(value.closeTimeUnix === null || isCount(value.closeTimeUnix))
    || !isNullableNumber(value.entryPrice) || !isNullableNumber(value.gateThreshold)
    || !isNullableNumber(value.tpPrice) || !isNullableNumber(value.slPrice)
    || !isNullableNumber(value.closePrice) || !isNullableNumber(value.riskReturn)
    || !isCount(value.evaluatedAtUnix) || !isCount(value.observedThroughUnix)
    || typeof value.isFinal !== "boolean") {
    throw new Error("Некорректный сохранённый результат сделки в ленте.");
  }
  return {
    status: value.status, entryTimeUnix: value.entryTimeUnix, entryPrice: value.entryPrice,
    gateThreshold: value.gateThreshold, tpPrice: value.tpPrice, slPrice: value.slPrice,
    closeTimeUnix: value.closeTimeUnix, closePrice: value.closePrice, riskReturn: value.riskReturn,
    evaluatedAtUnix: value.evaluatedAtUnix, observedThroughUnix: value.observedThroughUnix, isFinal: value.isFinal,
  };
}

/** One bounded page; a user operation owns serial continuation and keeps only counters/cursor. */
export async function refreshSetupOutcomesPage(cursor?: string, signal?: AbortSignal): Promise<SetupOutcomesRefreshPage> {
  const controller = new AbortController();
  const cancel = () => controller.abort();
  signal?.addEventListener("abort", cancel, { once: true });
  let timedOut = false;
  // Fifty setups may require historical reads. A stalled batch must still release its resources.
  const timer = setTimeout(() => { timedOut = true; controller.abort(); }, 120_000);
  try {
    if (signal?.aborted) controller.abort();
    controller.signal.throwIfAborted();
    const response = await apiFetch("/v1/setups/outcomes/refresh", {
      method: "POST", headers: { "content-type": "application/json" },
      body: JSON.stringify(cursor === undefined ? { limit: 50 } : { cursor, limit: 50 }),
      signal: controller.signal,
    });
    controller.signal.throwIfAborted();
    if (!response.ok) throw new Error(`Обновление результатов недоступно: HTTP ${response.status}.`);
    const body: unknown = await response.json();
    controller.signal.throwIfAborted();
    if (!isRecord(body) || !isCount(body.snapshotId) || !isCount(body.observedThroughUnix)
      || !isCount(body.processed) || body.processed > 50
      || !isCount(body.computed) || body.computed > body.processed
      || !isCount(body.updated) || body.updated > body.processed
      || !(body.nextCursor === null || (typeof body.nextCursor === "string" && body.nextCursor.length > 0 && body.nextCursor.length <= 4096))
      || (body.nextCursor !== null && (body.processed === 0 || body.nextCursor === cursor))) {
      throw new Error("Некорректный ответ обновления результатов: пакет не подтверждён.");
    }
    return {
      snapshotId: body.snapshotId, observedThroughUnix: body.observedThroughUnix,
      processed: body.processed, computed: body.computed, updated: body.updated, nextCursor: body.nextCursor,
    };
  } catch (cause) {
    if (timedOut && !signal?.aborted) throw new Error("Пакет обновления не подтверждён за 120 секунд. Повторите обновление вручную.", { cause });
    throw cause;
  } finally {
    clearTimeout(timer);
    signal?.removeEventListener("abort", cancel);
  }
}
