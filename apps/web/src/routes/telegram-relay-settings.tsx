import { useEffect, useRef, useState, type FormEvent } from "react";
import { Alert } from "../components/ui/alert";
import { Badge } from "../components/ui/badge";
import { Button } from "../components/ui/button";
import { Checkbox } from "../components/ui/checkbox";
import { Field } from "../components/ui/field";
import { Input } from "../components/ui/input";
import { SectionHeader } from "../components/ui/page-header";
import { Spinner } from "../components/ui/spinner";
import { formatIsoDateTime } from "../lib/datetime-format";
import { TelegramRelayApiError, getTelegramRelay, normalizeTelegramRelayEndpoint, probeTelegramRelay, saveTelegramRelay, testTelegramRelay,
  type TelegramRelayEnvelope, type TelegramRelayReason, type TelegramRelaySettings, type TelegramRelayStatus, type TelegramRelayTestResult } from "../lib/telegram-relay-api";
import { useRouteActive } from "./route-active";

type Action = "save" | "probe" | "test";
type BoundTest = TelegramRelayTestResult & { revision: number; endpointUrl: string };
const relayLabels: Record<TelegramRelayStatus["state"], string> = {
  disabled: "Доставка отключена", not_configured: "Relay не настроен", checking: "Проверка relay выполняется",
  connected: "Relay доступен · ключ подтверждён", error: "Ошибка соединения с relay", unknown: "Соединение с relay не подтверждено",
};
const reasons: Record<string, string> = {
  secret_not_configured: "Общий ключ подписи не настроен на основном сервере.",
  relay_timeout: "Relay не ответил за отведённое время.", relay_unreachable: "Relay недоступен по сети.",
  relay_auth_failed: "Relay не подтвердил общий ключ подписи.", relay_tls_error: "Не удалось подтвердить TLS-сертификат relay.",
  telegram_not_ready: "Relay пока не подтвердил готовность Telegram-канала.", telegram_forbidden: "У бота нет необходимых прав в Telegram-канале.",
};
function publicReason(value: TelegramRelayReason): string {
  return Object.hasOwn(reasons, value.code) ? reasons[value.code]! : "Сервер сообщил об ошибке relay или Telegram. Проверьте настройки и повторите явную проверку.";
}
function canonical(value: string): string { try { return normalizeTelegramRelayEndpoint(value); } catch { return value.trim(); } }
function matches(draft: TelegramRelaySettings, saved: TelegramRelaySettings): boolean {
  return draft.revision === saved.revision && draft.enabled === saved.enabled && canonical(draft.endpointUrl) === saved.endpointUrl;
}
function statusMatches(value: TelegramRelayEnvelope): boolean {
  return value.status.settingsRevision === value.settings.revision && value.status.endpointUrl === value.settings.endpointUrl;
}
function validation(draft: TelegramRelaySettings | null): string | null {
  if (!draft) return null;
  try { return draft.enabled && !normalizeTelegramRelayEndpoint(draft.endpointUrl) ? "Для включения Telegram укажите HTTPS-адрес relay." : null; }
  catch (cause) { return cause instanceof TelegramRelayApiError ? cause.message : "Проверьте HTTPS-адрес relay."; }
}

/** One bounded snapshot, draft and diagnostic result; no message history or automatic sends. */
export function TelegramRelaySettingsSection({ active }: { active: boolean }) {
  const routeActive = useRouteActive();
  const [snapshot, setSnapshot] = useState<TelegramRelayEnvelope | null>(null);
  const snapshotRef = useRef<TelegramRelayEnvelope | null>(null);
  const [draft, setDraft] = useState<TelegramRelaySettings | null>(null);
  const draftRef = useRef<TelegramRelaySettings | null>(null);
  const edited = useRef(false);
  const [available, setAvailable] = useState(false);
  const [loading, setLoading] = useState(false);
  const [busy, setBusy] = useState<Action | null>(null);
  const [readError, setReadError] = useState<string | null>(null);
  const [commandError, setCommandError] = useState<string | null>(null);
  const [message, setMessage] = useState<string | null>(null);
  const [lastTest, setLastTest] = useState<BoundTest | null>(null);
  const operations = useRef<{ run: (action?: Action) => void } | null>(null);

  useEffect(() => {
    if (!active || !routeActive) return;
    let disposed = false;
    let confirmed = false;
    let inFlight: { controller: AbortController; action: Action | undefined } | null = null;
    let timer: ReturnType<typeof setTimeout> | undefined;
    let refreshPending = false;
    const visible = () => !disposed && document.visibilityState !== "hidden";
    async function run(action?: Action) {
      if (!visible()) return;
      if (inFlight) { if (!action) refreshPending = true; return; }
      const saved = snapshotRef.current;
      const input = draftRef.current;
      if (action && (!confirmed || !saved || !input)) return;
      if (action && action !== "save" && (!saved || !input || !matches(input, saved.settings) || !statusMatches(saved)
        || !saved.settings.enabled || !saved.settings.endpointUrl || !saved.status.secretConfigured)) return;
      if (action === "save" && (!input || validation(input) || (saved && matches(input, saved.settings)))) return;
      if (action === "test" && typeof crypto.randomUUID !== "function") return;
      clearTimeout(timer); refreshPending = false;
      const current = { controller: new AbortController(), action }; inFlight = current;
      if (action) { confirmed = false; setAvailable(false); setBusy(action); setCommandError(null); setMessage(null); }
      else { setLoading(true); setAvailable(confirmed); setBusy(null); }
      let retryRead = true;
      let attemptedTest: BoundTest | null = null;
      try {
        if (action === "test" && saved) {
          attemptedTest = { requestId: crypto.randomUUID(), state: "unknown", reason: null, revision: saved.settings.revision, endpointUrl: saved.settings.endpointUrl };
          setLastTest(attemptedTest);
          const result = await testTelegramRelay(saved.settings.revision, attemptedTest.requestId, current.controller.signal);
          if (disposed || current.controller.signal.aborted) return;
          setLastTest({ ...attemptedTest, ...result });
        } else {
          const next = action === "save" && input ? await saveTelegramRelay(input, current.controller.signal)
            : action === "probe" && saved ? await probeTelegramRelay(saved.settings.revision, current.controller.signal) : await getTelegramRelay(current.controller.signal);
          if (disposed || current.controller.signal.aborted) return;
          if (snapshotRef.current && next.settings.revision < snapshotRef.current.settings.revision) throw new TelegramRelayApiError("Получена устаревшая ревизия relay. Ожидаем актуальный статус.");
          snapshotRef.current = next; setSnapshot(next);
          if (action === "save" || !edited.current) {
            const nextDraft = { ...next.settings }; draftRef.current = nextDraft; setDraft(nextDraft); edited.current = false;
          }
          if (action === "save") { setLastTest(null); setMessage("Настройки Telegram сохранены. Перезапуск сервисов не требуется."); }
          confirmed = true; setAvailable(visible()); setReadError(null);
        }
      } catch (cause) {
        if (!disposed && !current.controller.signal.aborted) {
          confirmed = false; setAvailable(false);
          retryRead = !(cause instanceof TelegramRelayApiError && [401, 403, 404, 405, 501].includes(cause.status ?? 0));
          const detail = cause instanceof TelegramRelayApiError ? cause.message : "Не удалось подтвердить команду Telegram relay.";
          if (action) setCommandError(detail); else setReadError(detail);
          if (attemptedTest) setLastTest({ ...attemptedTest, state: cause instanceof TelegramRelayApiError && [400, 401, 403, 404, 405, 409, 501].includes(cause.status ?? 0) ? "error" : "unknown" });
        }
      } finally {
        if (inFlight === current) inFlight = null;
        if (!disposed) {
          if (action) { setBusy(null); refreshPending = true; } else setLoading(false);
          if (visible() && refreshPending) { refreshPending = false; void run(); }
          else if (visible() && !current.controller.signal.aborted && retryRead) timer = setTimeout(() => { void run(); }, 15000);
        }
      }
    }
    const controls = { run: (action?: Action) => { void run(action); } }; operations.current = controls;
    function visibility() {
      clearTimeout(timer);
      if (document.visibilityState === "hidden") {
        confirmed = false; setAvailable(false);
        if (inFlight && !inFlight.action) inFlight.controller.abort();
      } else controls.run();
    }
    controls.run(); document.addEventListener("visibilitychange", visibility);
    return () => {
      disposed = true; inFlight?.controller.abort(); clearTimeout(timer);
      document.removeEventListener("visibilitychange", visibility);
      if (operations.current === controls) operations.current = null;
    };
  }, [active, routeActive]);

  function update(changes: Partial<Pick<TelegramRelaySettings, "enabled" | "endpointUrl">>) {
    const current = draftRef.current;
    if (!current) return;
    const next = { ...current, ...changes }; draftRef.current = next; setDraft(next);
    edited.current = !snapshotRef.current || !matches(next, snapshotRef.current.settings);
    setMessage(null);
  }
  function reset() {
    const current = snapshotRef.current;
    if (!current) return;
    const next = { ...current.settings }; draftRef.current = next; setDraft(next); edited.current = false;
    setCommandError(null); setMessage(null);
  }
  function save(event: FormEvent<HTMLFormElement>) { event.preventDefault(); operations.current?.run("save"); }
  const visible = active && routeActive && document.visibilityState !== "hidden";
  const dirty = Boolean(snapshot && draft && !matches(draft, snapshot.settings));
  const inputError = validation(draft);
  const bound = Boolean(snapshot && statusMatches(snapshot));
  const fresh = available && !dirty && bound;
  const blocked = !available ? "Загрузите актуальные настройки relay."
    : dirty ? "Сначала сохраните изменения или сбросьте черновик."
    : !snapshot?.settings.enabled ? "Включите Telegram и сохраните настройки."
    : !snapshot.settings.endpointUrl ? "Сначала сохраните HTTPS-адрес relay."
    : !snapshot.status.secretConfigured ? "Администратор должен настроить общий ключ подписи на основном сервере и relay; ключ не вводится в UI."
    : !bound ? "Статус относится к другому адресу или ревизии. Обновите статус." : null;
  const commandsDisabled = !visible || busy !== null || loading || blocked !== null;
  const effectiveState = !snapshot || !fresh ? "unknown" : !snapshot.settings.enabled ? "disabled"
    : !snapshot.settings.endpointUrl || !snapshot.status.secretConfigured ? "not_configured" : snapshot.status.state;
  const channel = fresh && snapshot?.settings.enabled && snapshot.status.secretConfigured && (effectiveState === "connected" || snapshot.status.telegramChannel === "error") ? snapshot.status.telegramChannel : "unknown";
  const testSupported = typeof crypto.randomUUID === "function";
  const displayedTest = !dirty && snapshot && lastTest?.revision === snapshot.settings.revision && lastTest.endpointUrl === snapshot.settings.endpointUrl ? lastTest : null;

  return <section aria-labelledby="telegram-relay-heading" className="grid gap-3">
    <SectionHeader id="telegram-relay-heading" title="Telegram relay" />
    <InfoNote>Удалённый relay отправляет текст в закрытый Telegram-канал. Токен бота и chat ID остаются только на relay; общий ключ подписи настраивается администратором в приватных файлах, не в браузере.</InfoNote>
    <form onSubmit={save} className="grid gap-3" aria-label="Настройки Telegram relay">
      <label className="flex items-center gap-2 text-sm"><Checkbox disabled={!draft || busy !== null} checked={draft?.enabled ?? false} onCheckedChange={(checked) => update({ enabled: checked === true })} />Включить Telegram</label>
      <Field label="HTTPS-адрес Telegram relay" error={inputError} hint="Рекомендуется домен. Допустим публичный IP с действительным TLS-сертификатом для этого IP. Только HTTPS origin без пути и параметров; публичность адреса и сертификат проверяет сервер.">
        {(control) => <Input {...control} type="url" inputMode="url" autoComplete="off" spellCheck={false} maxLength={2048} placeholder="https://relay.example/" disabled={!draft || busy !== null} value={draft?.endpointUrl ?? ""} onChange={(event) => update({ endpointUrl: event.target.value })} />}
      </Field>
      <div className="flex flex-wrap gap-2">
        <Button type="submit" disabled={!visible || !available || busy !== null || loading || !dirty || inputError !== null}>Сохранить Telegram relay</Button>
        {dirty && <Button type="button" variant="outline" disabled={!available || busy !== null} onClick={reset}>Сбросить изменения relay</Button>}
      </div>
    </form>
    {(loading || busy !== null) && <Spinner label={busy === "test" ? "Ожидание подтверждения теста Telegram" : busy === "probe" ? "Проверка соединения Telegram relay" : "Обновление настроек Telegram relay"} />}
    {snapshot && !fresh && <p role="status" className="text-sm">Показан последний подтверждённый снимок; соединение для текущего адреса и ревизии не подтверждено.</p>}
    <div className="grid gap-2 text-sm" aria-live="polite">
      <div><Badge variant={effectiveState === "connected" ? "success" : effectiveState === "error" ? "danger" : "neutral"}>{relayLabels[effectiveState]}</Badge></div>
      <p>Telegram-канал: {channel === "ready" ? "готовность подтверждена" : channel === "error" ? "ошибка доступа или отправки" : "готовность неизвестна"}.</p>
      <p>Ключ подписи на основном сервере: {!snapshot ? "неизвестно" : snapshot.status.secretConfigured ? "настроен" : "не настроен"}.</p>
      <p className="text-muted-foreground">Последняя проверка сохранённого адреса: {snapshot?.status.checkedAt ? formatIsoDateTime(snapshot.status.checkedAt) : "ещё не выполнялась"}</p>
      {fresh && snapshot?.status.reason && <p>{publicReason(snapshot.status.reason)}</p>}
    </div>
    {blocked && <p id="telegram-relay-blocker" className="text-sm text-muted-foreground">{blocked}</p>}
    <div className="flex flex-wrap gap-2">
      <Button type="button" variant="outline" disabled={commandsDisabled} aria-describedby={blocked ? "telegram-relay-blocker" : undefined} onClick={() => operations.current?.run("probe")}>Проверить Telegram relay</Button>
      <Button type="button" variant="outline" disabled={commandsDisabled || !testSupported} aria-describedby={blocked ? "telegram-relay-blocker" : undefined} onClick={() => operations.current?.run("test")}>Отправить тест в Telegram</Button>
      <Button type="button" variant="outline" disabled={!visible || busy !== null || loading} onClick={() => operations.current?.run()}>Обновить статус Telegram</Button>
    </div>
    {!testSupported && <p className="text-sm text-muted-foreground">Для безопасного идентификатора теста нужен браузер с crypto.randomUUID в защищённом контексте HTTPS (или localhost).</p>}
    {displayedTest && busy !== "test" && <Alert role={displayedTest.state === "sent" ? "status" : "alert"} variant={displayedTest.state === "sent" ? "success" : displayedTest.state === "error" ? "destructive" : "warning"}>
      {displayedTest.state === "sent" ? "Тестовое сообщение отправлено: Telegram подтвердил отправку. Это не подтверждает прочтение."
        : displayedTest.state === "error" ? "Тест завершился ошибкой. Автоматического повтора нет."
          : "Результат теста неизвестен: сообщение могло быть отправлено. Автоматического повтора нет; проверьте канал перед новой попыткой."}
      {displayedTest.reason && <> {publicReason(displayedTest.reason)}</>}
    </Alert>}
    {readError && <Alert variant="destructive" role="alert">{readError}</Alert>}
    {commandError && <Alert variant="destructive" role="alert">{commandError}</Alert>}
    {message && <Alert variant="success" role="status">{message}</Alert>}
    <InfoNote label="О проверке и доставке Telegram"><span className="grid gap-2"><span>Статус relay и права Telegram-канала — разные проверки. Проверка и один фиксированный тест выполняются только по нажатию. Чтение статуса ничего не отправляет.</span><span>Смена адреса не требует перезапуска сервисов. Доставляются только новые live-сетапы в пределах прежнего возраста уведомлений (по умолчанию 60 секунд от закрытия сигнальной минуты); история и replay не рассылаются, срок не продлевается.</span></span></InfoNote>
  </section>;
}
import { InfoNote } from "../components/ui/info-note";
