import { useEffect, useState, type FormEvent } from "react";
import { Button } from "../components/ui/button";
import { Input } from "../components/ui/input";
import { apiFetch, fetchPlatform, type PlatformState } from "../lib/platform-api";
import { AlgorithmSettings } from "./algorithm-settings";
import { errorText, localTime, writePlatform } from "./platform-controls";
import { LiveInstruments } from "./live-instruments";
import { OperationSettings } from "./operation-settings";
import { PushSettings } from "../components/push-settings";
import { BackupSettings } from "./backup-settings";

interface LiveStatus { enabled: boolean; running: boolean; lastScanAt: string | null; lastError: string | null; stale?: boolean; scanned?: number; setupCount?: number; instruments?: { symbol: string; state: string; lastClosedUnix?: number; error?: string }[] }
const instrumentStateLabels: Record<string, string> = { ready: "Готов", waiting_data: "Ожидание данных", insufficient_history: "Недостаточно истории", stale: "Устаревшие данные", error: "Ошибка" };

function LiveSettings() {
  const [state, setState] = useState<PlatformState<LiveStatus>>({ kind: "loading" });
  const [reload, setReload] = useState(0);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let cancelled = false;
    void fetchPlatform<LiveStatus>("/v1/live/status").then((result) => { if (!cancelled) setState(result); });
    return () => { cancelled = true; };
  }, [reload]);
  async function toggle(enabled: boolean) {
    setBusy(true); setError(null);
    try { await writePlatform("/v1/settings/live", { enabled }); setReload((value) => value + 1); }
    catch (cause) { setError(errorText(cause)); }
    finally { setBusy(false); }
  }
  return <section className="mt-8 grid max-w-xl gap-3" aria-labelledby="live-heading">
    <h2 id="live-heading" className="text-xl font-semibold">Live-сканирование</h2>
    {state.kind === "loading" && <p>Загрузка…</p>}
    {state.kind === "unreachable" && <p role="alert">Статус недоступен ({state.detail})</p>}
    {state.kind === "ready" && <>
      <p role="status">{state.data.enabled ? "Включено" : "Выключено"} · {state.data.running ? "Сканирование выполняется" : "Сканирование не выполняется"}</p>
      <p className="text-sm text-muted-foreground">Последнее сканирование: {state.data.lastScanAt ? new Date(state.data.lastScanAt).toLocaleString("ru-RU") : "ещё не выполнялось"}</p>
      {state.data.lastError && <p role="alert">{state.data.lastError}</p>}
      {state.data.stale === true && <p role="alert">Статус live устарел; текущее выполнение не подтверждено.</p>}
      {state.data.scanned !== undefined && <p className="text-sm">Проверено инструментов: {state.data.scanned}</p>}
      {state.data.setupCount !== undefined && <p className="text-sm">Сетапов в последнем отчёте: {state.data.setupCount}</p>}
      {state.data.instruments !== undefined && <details><summary className="cursor-pointer font-medium">Состояния live-инструментов ({state.data.instruments.length})</summary>
        <div className="mt-2 max-h-80 overflow-auto"><table className="w-full text-sm"><caption className="text-left text-muted-foreground">До 1000 строк из последнего ответа сервера</caption><thead><tr className="text-left"><th className="p-2">Инструмент</th><th className="p-2">Состояние</th><th className="p-2">Последняя закрытая минута</th><th className="p-2">Ошибка сервера</th></tr></thead><tbody>{state.data.instruments.slice(0, 1000).map((instrument, index) => <tr key={`${instrument.symbol}-${index}`} className="border-b border-input"><td className="p-2">{instrument.symbol}</td><td className="p-2">{instrumentStateLabels[instrument.state] ?? instrument.state}</td><td className="p-2">{instrument.lastClosedUnix === undefined ? "не сообщено" : localTime(instrument.lastClosedUnix)}</td><td className="max-w-sm break-words p-2">{instrument.error ?? "не сообщена"}</td></tr>)}</tbody></table></div>
        {state.data.instruments.length === 0 && <p className="text-sm">Сервер вернул пустой список состояний.</p>}
      </details>}
      <Button disabled={busy} onClick={() => { void toggle(!state.data.enabled); }}>{state.data.enabled ? "Отключить live" : "Включить live"}</Button>
    </>}
    {error && <p role="alert">{error}</p>}
    <Button variant="outline" disabled={busy} onClick={() => setReload((value) => value + 1)}>Обновить статус live</Button>
  </section>;
}

function CacheSettings() {
  const [state, setState] = useState<PlatformState<{ entries: number; bytes: number }>>({ kind: "loading" });
  const [reload, setReload] = useState(0);
  const [confirm, setConfirm] = useState(false);
  const [busy, setBusy] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  useEffect(() => {
    let cancelled = false;
    void fetchPlatform<{ entries: number; bytes: number }>("/v1/replay-cache/stats").then((result) => { if (!cancelled) setState(result); });
    return () => { cancelled = true; };
  }, [reload]);
  async function clear() {
    setBusy(true); setError(null); setMessage(null);
    try {
      const response = await apiFetch("/v1/replay-cache", { method: "DELETE" });
      if (!response.ok) throw new Error(`Не удалось очистить кеш: HTTP ${response.status}`);
      const result = await response.json() as { removed: number };
      setMessage(`Удалено записей кеша: ${result.removed}.`); setConfirm(false); setReload((value) => value + 1);
    } catch (cause) { setError(errorText(cause)); }
    finally { setBusy(false); }
  }
  return <section className="mt-8 grid max-w-xl gap-3" aria-labelledby="cache-heading">
    <h2 id="cache-heading" className="text-xl font-semibold">Кеш исторических запусков</h2>
    {state.kind === "loading" && <p>Загрузка…</p>}
    {state.kind === "unreachable" && <p role="alert">Кеш недоступен ({state.detail})</p>}
    {state.kind === "ready" && <p>Записей: {state.data.entries} · Размер таблицы: {(state.data.bytes / 1024 / 1024).toFixed(2)} МиБ</p>}
    <p className="text-sm text-muted-foreground">Очистка удаляет только кеш расчётов. Сохранённые запуски и рыночная история остаются; повторный расчёт может занять больше времени.</p>
    <label className="flex items-center gap-2 text-sm"><Input type="checkbox" className="h-4 w-4" checked={confirm} onChange={(event) => setConfirm(event.target.checked)} />Подтверждаю очистку всего кеша</label>
    <Button variant="outline" disabled={busy || !confirm || state.kind !== "ready"} onClick={() => { void clear(); }}>Очистить кеш</Button>
    {message && <p role="status">{message}</p>}{error && <p role="alert">{error}</p>}
  </section>;
}

export function SettingsPage() {
  const [maxAge, setMaxAge] = useState("");
  const [ready, setReady] = useState(false);
  const [message, setMessage] = useState<string | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [saving, setSaving] = useState(false);
  useEffect(() => {
    let cancelled = false;
    void fetchPlatform<{ maxAgeSeconds: number }>("/v1/settings/notifications").then((state) => {
      if (cancelled) return;
      if (state.kind === "ready") { setMaxAge(String(state.data.maxAgeSeconds)); setReady(true); }
      else if (state.kind === "unreachable") setError(`Платформа недоступна (${state.detail})`);
    });
    return () => { cancelled = true; };
  }, []);
  async function save(event: FormEvent<HTMLFormElement>) {
    event.preventDefault(); setError(null); setMessage(null);
    const value = Number(maxAge);
    if (!maxAge.trim() || !Number.isInteger(value) || value < 1) { setError("Укажите положительное целое число секунд."); return; }
    setSaving(true);
    try { await writePlatform("/v1/settings/notifications", { maxAgeSeconds: value }); setMessage("Настройки уведомлений сохранены."); }
    catch (cause) { setError(errorText(cause)); }
    finally { setSaving(false); }
  }
  return <section aria-labelledby="settings-heading">
    <h1 id="settings-heading" className="text-2xl font-semibold">Настройки</h1>
    <AlgorithmSettings />
    <LiveSettings />
    <LiveInstruments />
    <form className="mt-8 grid max-w-xl gap-3" onSubmit={(event) => { void save(event); }}>
      <h2 className="text-xl font-semibold">Уведомления</h2>
      <p className="text-sm text-muted-foreground">Telegram не подключён. Настройка возраста задаёт порог свежести; подписка Web Push включается отдельно ниже и только по вашему действию.</p>
      <p className="text-sm text-muted-foreground">Web Push недоступен по обычному HTTP: требуется HTTPS (исключение — localhost). Сохранение порога не запрашивает разрешение на уведомления.</p>
      <label className="grid gap-1 text-sm">Максимальный возраст уведомления (секунд)<Input type="number" min="1" step="1" required disabled={!ready} value={maxAge} onChange={(event) => { setMaxAge(event.target.value); setMessage(null); }} /></label>
      <p className="text-xs text-muted-foreground">Просроченные сетапы остаются в ленте, но не рассылаются как свежие.</p>
      {error && <p role="alert">{error}</p>}{message && <p role="status">{message}</p>}
      <Button type="submit" disabled={saving || !ready}>{saving ? "Сохранение…" : "Сохранить уведомления"}</Button>
    </form>
    <PushSettings />
    <CacheSettings />
    <OperationSettings />
    <BackupSettings />
  </section>;
}
