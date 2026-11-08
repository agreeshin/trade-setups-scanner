import { useEffect, useState, type FormEvent } from "react";
import { Alert } from "../components/ui/alert";
import { Button } from "../components/ui/button";
import { Card } from "../components/ui/card";
import { Checkbox } from "../components/ui/checkbox";
import { Field } from "../components/ui/field";
import { Input } from "../components/ui/input";
import { PageHeader, SectionHeader } from "../components/ui/page-header";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "../components/ui/table";
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
  return <section className="mt-10 grid max-w-xl gap-3" aria-labelledby="live-heading">
    <SectionHeader id="live-heading" title="Live-сканирование" />
    <Card className="grid gap-3">
      {state.kind === "loading" && <p className="text-sm text-muted-foreground">Загрузка…</p>}
      {state.kind === "unreachable" && <Alert variant="destructive" role="alert">Статус недоступен ({state.detail})</Alert>}
      {state.kind === "ready" && <>
        <p role="status">{state.data.enabled ? "Включено" : "Выключено"} · {state.data.running ? "Сканирование выполняется" : "Сканирование не выполняется"}</p>
        <p className="text-sm text-muted-foreground">Последнее сканирование: {state.data.lastScanAt ? new Date(state.data.lastScanAt).toLocaleString("ru-RU") : "ещё не выполнялось"}</p>
        {state.data.lastError && <Alert variant="destructive" role="alert">{state.data.lastError}</Alert>}
        {state.data.stale === true && <Alert variant="warning" role="alert">Статус live устарел; текущее выполнение не подтверждено.</Alert>}
        {state.data.scanned !== undefined && <p className="text-sm">Проверено инструментов: {state.data.scanned}</p>}
        {state.data.setupCount !== undefined && <p className="text-sm">Сетапов в последнем отчёте: {state.data.setupCount}</p>}
        {state.data.instruments !== undefined && <details><summary className="cursor-pointer font-medium">Состояния live-инструментов ({state.data.instruments.length})</summary>
          <div className="mt-2 max-h-80 overflow-auto"><Table>
            <caption className="text-left text-muted-foreground">До 1000 строк из последнего ответа сервера</caption>
            <TableHeader><TableRow><TableHead>Инструмент</TableHead><TableHead>Состояние</TableHead><TableHead>Последняя закрытая минута</TableHead><TableHead>Ошибка сервера</TableHead></TableRow></TableHeader>
            <TableBody>{state.data.instruments.slice(0, 1000).map((instrument, index) => <TableRow key={`${instrument.symbol}-${index}`}>
              <TableCell className="font-medium">{instrument.symbol}</TableCell>
              <TableCell>{instrumentStateLabels[instrument.state] ?? instrument.state}</TableCell>
              <TableCell>{instrument.lastClosedUnix === undefined ? "не сообщено" : localTime(instrument.lastClosedUnix)}</TableCell>
              <TableCell className="max-w-sm break-words">{instrument.error ?? "не сообщена"}</TableCell>
            </TableRow>)}</TableBody>
          </Table></div>
          {state.data.instruments.length === 0 && <p className="text-sm">Сервер вернул пустой список состояний.</p>}
        </details>}
        <div><Button disabled={busy} onClick={() => { void toggle(!state.data.enabled); }}>{state.data.enabled ? "Отключить live" : "Включить live"}</Button></div>
      </>}
      {error && <Alert variant="destructive" role="alert">{error}</Alert>}
      <div><Button variant="outline" disabled={busy} onClick={() => setReload((value) => value + 1)}>Обновить статус live</Button></div>
    </Card>
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
  return <section className="mt-10 grid max-w-xl gap-3" aria-labelledby="cache-heading">
    <SectionHeader id="cache-heading" title="Кеш исторических запусков" />
    <Card className="grid gap-3">
      {state.kind === "loading" && <p className="text-sm text-muted-foreground">Загрузка…</p>}
      {state.kind === "unreachable" && <Alert variant="destructive" role="alert">Кеш недоступен ({state.detail})</Alert>}
      {state.kind === "ready" && <p>Записей: {state.data.entries} · Размер таблицы: {(state.data.bytes / 1024 / 1024).toFixed(2)} МиБ</p>}
      <p className="text-sm text-muted-foreground">Очистка удаляет только кеш расчётов. Сохранённые запуски и рыночная история остаются; повторный расчёт может занять больше времени.</p>
      <label className="flex items-center gap-2 text-sm"><Checkbox checked={confirm} onCheckedChange={(checked) => setConfirm(checked === true)} />Подтверждаю очистку всего кеша</label>
      <div><Button variant="outline" disabled={busy || !confirm || state.kind !== "ready"} onClick={() => { void clear(); }}>Очистить кеш</Button></div>
      {message && <Alert variant="success" role="status">{message}</Alert>}
      {error && <Alert variant="destructive" role="alert">{error}</Alert>}
    </Card>
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
    <PageHeader id="settings-heading" title="Настройки" />
    <AlgorithmSettings />
    <LiveSettings />
    <LiveInstruments />
    <form className="mt-10 grid max-w-xl gap-3" onSubmit={(event) => { void save(event); }} aria-labelledby="notifications-heading">
      <SectionHeader id="notifications-heading" title="Уведомления" />
      <Card className="grid gap-3">
        <p className="text-sm text-muted-foreground">Telegram не подключён. Настройка возраста задаёт порог свежести; подписка Web Push включается отдельно ниже и только по вашему действию.</p>
        <p className="text-sm text-muted-foreground">Web Push недоступен по обычному HTTP: требуется HTTPS (исключение — localhost). Сохранение порога не запрашивает разрешение на уведомления.</p>
        <Field label="Максимальный возраст уведомления (секунд)">
          {(control) => <Input type="number" min="1" step="1" required disabled={!ready} value={maxAge} onChange={(event) => { setMaxAge(event.target.value); setMessage(null); }} {...control} />}
        </Field>
        <p className="text-xs text-muted-foreground">Просроченные сетапы остаются в ленте, но не рассылаются как свежие.</p>
        {error && <Alert variant="destructive" role="alert">{error}</Alert>}
        {message && <Alert variant="success" role="status">{message}</Alert>}
        <div><Button type="submit" disabled={saving || !ready}>{saving ? "Сохранение…" : "Сохранить уведомления"}</Button></div>
      </Card>
    </form>
    <PushSettings />
    <CacheSettings />
    <OperationSettings />
    <BackupSettings />
  </section>;
}
