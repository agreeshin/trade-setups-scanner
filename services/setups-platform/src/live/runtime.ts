import type { FastifyInstance } from "fastify";
import type { Kysely } from "kysely";
import type { SignalsDB } from "../db/schema.js";
import { getAlgorithm } from "../algorithms.js";
import { getSetting, setSetting, getNotificationMaxAgeSeconds } from "../settings.js";
import { fetchCatalog } from "../platform-catalog.js";
import { ApiCandleSource } from "../replay/source.js";
import { liveScanOnce } from "./live-scanner.js";
import { createHash } from "node:crypto";
import { createPushNotifier } from "../web-push.js";

/** One in-flight scan, bounded page buffers, no retained per-symbol candle histories. */
export function startLiveRuntime(app: FastifyInstance, db: Kysely<SignalsDB>, marketData: { url: string; apiKey: string }): { runOnce: () => Promise<void> } {
  const cancel = { cancelled: false };
  let inFlight: Promise<void> | undefined;
  let lastMinute = 0;
  async function tick(): Promise<void> {
    if ((await getSetting(db, "live")).enabled !== true) return;
    const algorithm = await getAlgorithm(db);
    if (!algorithm?.enabled) return;
    const nowUnix = Math.floor(Date.now() / 60000) * 60;
    if (nowUnix === lastMinute) return;
    const startedAt = new Date().toISOString();
    const configHash = createHash("sha256").update(JSON.stringify([algorithm.version, algorithm.params])).digest("hex");
    await setSetting(db, "live-status", { running: true, lastScanAt: startedAt, lastError: null });
    const list = await db.selectFrom("instrument_lists").where("list_type", "=", "live").selectAll().execute();
    const configured = (await getSetting(db, "instrument-list:live")).configured === true;
    const selected = new Set(list.map((item) => `${item.exchange_code}/${item.category}/${item.symbol}`));
    const instruments = (await fetchCatalog(marketData.url, marketData.apiKey)).filter((item) => item.status === "active" && (!configured || selected.has(`${item.exchange}/${item.category}/${item.symbol}`)));
    if (instruments.length > 1000) throw new Error("select at most 1000 live instruments");
    const instrumentStates: { symbol: string; state: string; lastClosedUnix?: number; error?: string }[] = [];
    let scanned = 0;
    let setupCount = 0;
    let stale = 0;
    let failed = 0;
    for (const instrument of instruments) {
      if (cancel.cancelled || (await getSetting(db, "live")).enabled !== true) break;
      try {
      const source = new ApiCandleSource(marketData.url, marketData.apiKey, instrument.id);
      const recent = await source.read({ fromUnix: nowUnix - 86400, toUnix: nowUnix, limit: 1440 });
      const lastBar = recent.bars.at(-1);
      if (!lastBar) { stale += 1; instrumentStates.push({ symbol: instrument.symbol, state: "waiting_data" }); continue; }
      const latestClose = lastBar.openTimeUnix + 60;
      if (nowUnix - latestClose > 60) stale += 1;
      const checkpointKey = `live-checkpoint:${instrument.id}`;
      const checkpoint = await getSetting(db, checkpointKey);
      const fromUnix = checkpoint.configHash === configHash && typeof checkpoint.nextUnix === "number"
        ? checkpoint.nextUnix : latestClose - 600;
      if (fromUnix >= latestClose) {
        instrumentStates.push({ symbol: instrument.symbol, state: nowUnix - latestClose > 60 ? "stale" : "waiting_data", lastClosedUnix: latestClose });
        continue;
      }
      // Catch up in bounded one-day chunks; retain the next boundary across restart.
      const toUnix = Math.min(latestClose, fromUnix + 86400);
      const maxAgeSeconds = await getNotificationMaxAgeSeconds(db);
      const identity = { exchangeCode: instrument.exchange, category: instrument.category, symbol: instrument.symbol };
      const result = await liveScanOnce({
        db, source,
        instruments: [identity],
        nowUnix: toUnix, fromUnix, deliveryNowUnix: nowUnix, params: algorithm.params, cancel,
        maxAgeSeconds, notifier: createPushNotifier(db, identity, maxAgeSeconds),
      });
      scanned += 1;
      setupCount += result.newSetups.length;
      instrumentStates.push({ symbol: instrument.symbol, state: !result.ready ? "insufficient_history" : nowUnix - latestClose > 60 ? "stale" : "ready", lastClosedUnix: latestClose });
      if (!cancel.cancelled && result.lastProcessedUnix !== null) {
        await setSetting(db, checkpointKey, { configHash, nextUnix: result.lastProcessedUnix + 60 });
      }
      } catch {
        failed += 1;
        instrumentStates.push({ symbol: instrument.symbol, state: "error", error: "Ошибка чтения или расчёта" });
        app.log.warn({ symbol: instrument.symbol }, "live instrument scan failed");
      }
    }
    lastMinute = nowUnix;
    await setSetting(db, "live-status", { running: false, lastScanAt: startedAt, lastError: failed ? `Не обработано инструментов: ${failed}` : null, scanned, setupCount, stale, instruments: instrumentStates });
    app.log.info({ scanned, setupCount }, "live scan completed");
  }
  function runOnce(): Promise<void> {
    if (inFlight) return inFlight;
    if (cancel.cancelled) return Promise.resolve();
    inFlight = tick().catch(async () => {
      app.log.error("live scan failed; check market-data readiness");
      await setSetting(db, "live-status", { running: false, lastScanAt: new Date().toISOString(), lastError: "Ошибка сканирования; проверьте сервис данных" }).catch(() => undefined);
    }).finally(() => { inFlight = undefined; });
    return inFlight;
  }
  const timer = setInterval(() => { void runOnce(); }, 15000);
  timer.unref();
  app.addHook("onClose", async () => {
    clearInterval(timer);
    cancel.cancelled = true;
    await inFlight;
    await setSetting(db, "live-status", { running: false, lastScanAt: null, lastError: null });
  });
  return { runOnce };
}
