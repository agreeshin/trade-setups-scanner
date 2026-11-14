import { useEffect, useState, type FormEvent } from "react";
import { Alert } from "../components/ui/alert";
import { Button } from "../components/ui/button";
import { Checkbox } from "../components/ui/checkbox";
import { Field } from "../components/ui/field";
import { Input } from "../components/ui/input";
import { PageHeader, SectionHeader } from "../components/ui/page-header";
import { PageSidebar, PageSidebarPanel } from "../components/ui/page-sidebar";
import { Select } from "../components/ui/select";
import { Separator } from "../components/ui/separator";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "../components/ui/table";
import { applyInterfaceSettings, effectiveTimezone, formatIsoDateTime, formatUnix, interfaceSettings, type DateOrder, type DateSeparator, type InterfaceSettings } from "../lib/datetime-format";
import { apiFetch, fetchPlatform, type PlatformState } from "../lib/platform-api";
import { errorText, localTime, savePreference, writePlatform } from "./platform-controls";
import { LiveInstruments } from "./live-instruments";
import { OperationSettings } from "./operation-settings";
import { ContinuousMaintenance } from "./continuous-maintenance";
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
  return <section className="grid gap-3" aria-labelledby="live-heading">
    <SectionHeader id="live-heading" title="Live-сканирование" />
    <div className="grid gap-3">
      {state.kind === "loading" && <p className="text-sm text-muted-foreground">Загрузка…</p>}
      {state.kind === "unreachable" && <Alert variant="destructive" role="alert">Статус недоступен ({state.detail})</Alert>}
      {state.kind === "ready" && <>
        <p role="status">{state.data.enabled ? "Включено" : "Выключено"} · {state.data.running ? "Сканирование выполняется" : "Сканирование не выполняется"}</p>
        <p className="text-sm text-muted-foreground">Последнее сканирование: {state.data.lastScanAt ? formatIsoDateTime(state.data.lastScanAt) : "ещё не выполнялось"}</p>
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
    </div>
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
  return <section className="grid gap-3" aria-labelledby="cache-heading">
    <SectionHeader id="cache-heading" title="Кеш исторических запусков" />
    <div className="grid gap-3">
      {state.kind === "loading" && <p className="text-sm text-muted-foreground">Загрузка…</p>}
      {state.kind === "unreachable" && <Alert variant="destructive" role="alert">Кеш недоступен ({state.detail})</Alert>}
      {state.kind === "ready" && <p>Записей: {state.data.entries} · Размер таблицы: {(state.data.bytes / 1024 / 1024).toFixed(2)} МиБ</p>}
      <p className="text-sm text-muted-foreground">Очистка удаляет только кеш расчётов. Сохранённые запуски и рыночная история остаются; повторный расчёт может занять больше времени.</p>
      <label className="flex items-center gap-2 text-sm"><Checkbox checked={confirm} onCheckedChange={(checked) => setConfirm(checked === true)} />Подтверждаю очистку всего кеша</label>
      <div><Button variant="outline" disabled={busy || !confirm || state.kind !== "ready"} onClick={() => { void clear(); }}>Очистить кеш</Button></div>
      {message && <Alert variant="success" role="status">{message}</Alert>}
      {error && <Alert variant="destructive" role="alert">{error}</Alert>}
    </div>
  </section>;
}

const SAMPLE_UNIX = Date.UTC(2026, 0, 9, 14, 5, 7) / 1000;

const TIMEZONES = [
  "UTC", "Europe/Moscow", "Europe/Kaliningrad", "Europe/London", "Europe/Berlin", "Europe/Istanbul",
  "Asia/Dubai", "Asia/Almaty", "Asia/Yekaterinburg", "Asia/Novosibirsk", "Asia/Vladivostok",
  "America/New_York", "America/Chicago", "America/Los_Angeles",
];

function InterfaceSettingsSection() {
  const [settings, setSettings] = useState<InterfaceSettings>(() => interfaceSettings());
  const [saving, setSaving] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [message, setMessage] = useState<string | null>(null);

  function update(changes: Partial<InterfaceSettings>) {
    setSettings(applyInterfaceSettings({ ...settings, ...changes }));
    setMessage(null);
  }

  async function save() {
    setSaving(true); setError(null);
    try {
      await savePreference("interface", settings);
      setMessage("Настройки интерфейса сохранены; страница сейчас обновится.");
      window.setTimeout(() => { window.location.reload(); }, 500);
    } catch (cause) {
      setError(errorText(cause));
      setSaving(false);
    }
  }

  return <section className="grid gap-3" aria-labelledby="interface-heading">
    <SectionHeader id="interface-heading" title="Интерфейс" />
    <div className="grid gap-3">
      <p className="text-sm text-muted-foreground">Формат применяется ко всем датам и времени в приложении и сохраняется для всех ваших устройств. Нативные селекторы дат следуют локали браузера.</p>
      <Field label="Порядок даты">
        {(control) => <Select {...control} value={settings.dateOrder} onChange={(event) => update({ dateOrder: event.target.value as DateOrder })}>
          <option value="dmy">День · месяц · год</option>
          <option value="mdy">Месяц · день · год</option>
          <option value="ymd">Год · месяц · день</option>
        </Select>}
      </Field>
      <Field label="Разделитель даты">
        {(control) => <Select {...control} value={settings.dateSeparator} onChange={(event) => update({ dateSeparator: event.target.value as DateSeparator })}>
          <option value=".">Точка (09.01.2026)</option>
          <option value="/">Слэш (09/01/2026)</option>
          <option value="-">Дефис (09-01-2026)</option>
        </Select>}
      </Field>
      <Field label="Год">
        {(control) => <Select {...control} value={String(settings.yearDigits)} onChange={(event) => update({ yearDigits: event.target.value === "2" ? 2 : 4 })}>
          <option value="4">Четыре цифры (2026)</option>
          <option value="2">Две цифры (26)</option>
        </Select>}
      </Field>
      <Field label="Формат времени">
        {(control) => <Select {...control} value={settings.time24 ? "24" : "12"} onChange={(event) => update({ time24: event.target.value === "24" })}>
          <option value="24">24-часовой (14:05:07)</option>
          <option value="12">12-часовой (02:05:07 PM)</option>
        </Select>}
      </Field>
      <Field label="Часовой пояс">
        {(control) => <Select {...control} value={settings.timezone} onChange={(event) => update({ timezone: event.target.value })}>
          <option value="auto">Авто (системный: {Intl.DateTimeFormat().resolvedOptions().timeZone})</option>
          {TIMEZONES.map((zone) => <option key={zone} value={zone}>{zone}</option>)}
        </Select>}
      </Field>
      <p role="status" className="text-sm">Пример: {formatUnix(SAMPLE_UNIX)} ({effectiveTimezone()})</p>
      {error && <Alert variant="destructive" role="alert">{error}</Alert>}
      {message && <Alert variant="success" role="status">{message}</Alert>}
      <div><Button disabled={saving} onClick={() => { void save(); }}>{saving ? "Сохранение…" : "Сохранить и применить"}</Button></div>
    </div>
  </section>;
}

type SettingsSection = "scanning" | "notifications" | "maintenance" | "interface";

export function SettingsPage() {
  const [section, setSection] = useState<SettingsSection>("scanning");
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
    <PageSidebar label="Подразделы настроек" value={section} onValueChange={setSection} items={[
      { id: "scanning", label: "Сканирование" }, { id: "notifications", label: "Уведомления" },
      { id: "maintenance", label: "Обслуживание" }, { id: "interface", label: "Интерфейс" },
    ]}>
    <PageSidebarPanel value="scanning"><LiveSettings /><Separator /><LiveInstruments /></PageSidebarPanel>
    <PageSidebarPanel value="notifications">
      <form className="grid gap-3" onSubmit={(event) => { void save(event); }} aria-labelledby="notifications-heading">
        <SectionHeader id="notifications-heading" title="Уведомления" />
        <div className="grid gap-3">
          <p className="text-sm text-muted-foreground">Telegram не подключён. Настройка возраста задаёт порог свежести; подписка Web Push включается отдельно ниже и только по вашему действию.</p>
          <p className="text-sm text-muted-foreground">Web Push недоступен по обычному HTTP: требуется HTTPS (исключение — localhost). Сохранение порога не запрашивает разрешение на уведомления.</p>
          <Field label="Максимальный возраст уведомления (секунд)">
            {(control) => <Input type="number" min="1" step="1" required disabled={!ready} value={maxAge} onChange={(event) => { setMaxAge(event.target.value); setMessage(null); }} {...control} />}
          </Field>
          <p className="text-xs text-muted-foreground">Просроченные сетапы остаются в ленте, но не рассылаются как свежие.</p>
          {error && <Alert variant="destructive" role="alert">{error}</Alert>}
          {message && <Alert variant="success" role="status">{message}</Alert>}
          <div><Button type="submit" disabled={saving || !ready}>{saving ? "Сохранение…" : "Сохранить уведомления"}</Button></div>
        </div>
      </form>
      <Separator /><PushSettings />
    </PageSidebarPanel>
    <PageSidebarPanel value="maintenance"><ContinuousMaintenance active={section === "maintenance"} /><Separator /><CacheSettings /><Separator /><OperationSettings /><Separator /><BackupSettings /></PageSidebarPanel>
    <PageSidebarPanel value="interface"><InterfaceSettingsSection /></PageSidebarPanel>
    </PageSidebar>
  </section>;
}
