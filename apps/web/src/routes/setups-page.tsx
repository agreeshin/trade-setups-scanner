import { useEffect, useState } from "react";
import { Alert, AlertDescription } from "../components/ui/alert";
import { Button } from "../components/ui/button";
import { Card, CardContent } from "../components/ui/card";
import { Field } from "../components/ui/field";
import { Input } from "../components/ui/input";
import { PageHeader } from "../components/ui/page-header";
import { Pagination } from "../components/ui/pagination";
import { Spinner } from "../components/ui/spinner";
import { PLATFORM_URL, fetchPlatform } from "../lib/platform-api";
import { errorText, localTime, savePreference, type SetupRow } from "./platform-controls";
import { useRouteActive } from "./route-active";
import { SetupTable } from "./setup-table";

interface SetupPage { setups: SetupRow[]; offset: number; limit: number; total: number; snapshotId: number }
interface FeedQuery { filter: string; offset: number; snapshotId?: number; mode: "live" | "paused" | "history" }
interface FeedState { page: SetupPage | null; dataQuery: FeedQuery | null; responseQuery: FeedQuery | null; readAt: number | null; error: string | null }

export function SetupsPage() {
  const active = useRouteActive();
  // One replaceable page and one table-owned selected recommendation; no growing live history.
  const [state, setState] = useState<FeedState>({ page: null, dataQuery: null, responseQuery: null, readAt: null, error: null });
  const [query, setQuery] = useState<FeedQuery>({ filter: "", offset: 0, mode: "live" });
  const [preferencesReady, setPreferencesReady] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [message, setMessage] = useState<string | null>(null);
  useEffect(() => {
    if (!preferencesReady || !active) return;
    let disposed = false;
    let request: AbortController | undefined;
    let timer: ReturnType<typeof setTimeout> | undefined;
    const params = new URLSearchParams({ offset: String(query.offset), limit: "25", q: query.filter });
    if (query.snapshotId !== undefined) params.set("snapshotId", String(query.snapshotId));
    const following = query.mode === "live" && query.offset === 0 && query.snapshotId === undefined;
    async function load() {
      if (disposed || document.visibilityState === "hidden") return;
      request?.abort(); clearTimeout(timer);
      const current = new AbortController(); request = current;
      try {
        const result = await fetchPlatform<SetupPage>(`/v1/setups?${params}`, current.signal);
        if (disposed || current.signal.aborted) return;
        if (result.kind === "ready") {
          const seen = new Set<number>();
          const setups = result.data.setups.filter((setup) => {
            if (setup.id === undefined) return true;
            if (seen.has(setup.id)) return false;
            seen.add(setup.id); return true;
          });
          setState({ page: { ...result.data, setups }, dataQuery: query, responseQuery: query, readAt: Date.now() / 1000, error: null });
        } else if (result.kind === "unreachable") {
          setState((previous) => ({ ...previous, responseQuery: query, error: result.detail }));
        }
      } catch (cause) {
        if (!disposed && !current.signal.aborted) setState((previous) => ({ ...previous, responseQuery: query, error: errorText(cause) }));
      } finally {
        // Serial polling retries transient errors too. Hidden/inactive views own no request or timer.
        if (!disposed && !current.signal.aborted && following) timer = setTimeout(() => { void load(); }, 3000);
      }
    }
    function visibility() {
      request?.abort(); clearTimeout(timer);
      if (document.visibilityState !== "hidden") void load();
    }
    void load(); document.addEventListener("visibilitychange", visibility);
    return () => { disposed = true; request?.abort(); clearTimeout(timer); document.removeEventListener("visibilitychange", visibility); };
  }, [query, preferencesReady, active]);
  useEffect(() => {
    const controller = new AbortController();
    void fetchPlatform<{ value: { setupFilter?: string } }>("/v1/settings/ui", controller.signal).then((result) => {
      if (controller.signal.aborted) return;
      if (result.kind === "ready" && typeof result.data.value.setupFilter === "string") setQuery({ filter: result.data.value.setupFilter.slice(0, 200), offset: 0, mode: "live" });
      else if (result.kind === "unreachable") setError(`Сохранённый фильтр недоступен (${result.detail})`);
      setPreferencesReady(true);
    });
    return () => controller.abort();
  }, []);
  async function saveFilter() {
    setBusy(true); setError(null); setMessage(null);
    try { await savePreference("setupFilter", query.filter); setMessage("Фильтр сохранён в базе сигналов."); }
    catch (cause) { setError(errorText(cause)); }
    finally { setBusy(false); }
  }
  function loadPage(offset: number) {
    if (!state.page || state.dataQuery !== query) return;
    setQuery({ ...query, offset, snapshotId: state.page.snapshotId, mode: "history" });
  }
  const page = state.page;
  const pages = page ? Math.max(1, Math.ceil(page.total / page.limit)) : 1;
  const currentPage = page ? Math.floor(page.offset / page.limit) : 0;
  const changingPage = state.dataQuery !== query;
  return <section aria-labelledby="setups-heading" className="grid gap-4">
    <PageHeader id="setups-heading" title="Сетапы" description="Поиск по всей сохранённой live-ленте. Первая страница follow-live обновляется каждые 3 секунды в активной видимой вкладке. Исторические страницы сохраняют снимок." />
    <Card>
      <CardContent className="flex flex-wrap items-end gap-3">
        <Field label="Инструмент, биржа или алгоритм" className="w-full max-w-sm">
          {(control) => <Input {...control} disabled={!preferencesReady} value={query.filter} maxLength={200} onChange={(event) => { setQuery({ filter: event.target.value, offset: 0, mode: query.mode === "paused" ? "paused" : "live" }); setMessage(null); }} />}
        </Field>
        <div className="flex flex-wrap gap-2">
          <Button variant="outline" disabled={busy || !preferencesReady} onClick={() => { void saveFilter(); }}>Сохранить фильтр</Button>
          <Button variant="outline" disabled={!preferencesReady} onClick={() => setQuery({ filter: query.filter, offset: 0, mode: query.mode === "paused" ? "paused" : "live" })}>Обновить</Button>
          {query.mode === "live" ? <Button variant="outline" disabled={!page || changingPage} onClick={() => { if (page) setQuery({ ...query, snapshotId: page.snapshotId, mode: "paused" }); }}>Приостановить обновление</Button>
            : <Button variant="outline" disabled={!preferencesReady} onClick={() => setQuery({ filter: query.filter, offset: 0, mode: "live" })}>Следить за live-лентой</Button>}
          <Button asChild variant="outline"><a href={`${PLATFORM_URL}/v1/export/setups.csv`}>Экспорт CSV (без фильтра)</a></Button>
        </div>
        <p className="w-full text-xs text-muted-foreground" aria-live="polite">{query.mode === "live" ? "Live-лента · обновление каждые 3 с в активной видимой вкладке." : query.mode === "history" ? "Исторический снимок · автообновление приостановлено." : "Обновление ленты приостановлено."}</p>
        <p className="w-full text-xs text-muted-foreground">CSV содержит всю сохранённую live-ленту на начало выгрузки, без фильтра экрана и лимита строк. Статус и отмена доступны в загрузках браузера.</p>
      </CardContent>
    </Card>
    {error && <Alert variant="destructive" role="alert"><AlertDescription>{error}</AlertDescription></Alert>}
    {message && <Alert variant="success" role="status"><AlertDescription>{message}</AlertDescription></Alert>}
    {state.readAt !== null && <p className="text-xs text-muted-foreground">Последнее успешное чтение: {localTime(state.readAt)}</p>}
    {state.error && <Alert variant="destructive" role="alert">{page ? `Ошибка обновления (${state.error}). Показан последний подтверждённый список; данные могли устареть.` : `Лента недоступна (${state.error})`}</Alert>}
    {preferencesReady && state.responseQuery !== query && <Spinner label={page ? "Загрузка выбранной страницы; пока показан предыдущий список…" : "Загрузка…"} />}
    {!preferencesReady && <Spinner label="Загрузка…" />}
    {page && <>
      <SetupTable setups={page.setups} />
      <Pagination
        label="Страницы сетапов"
        page={currentPage + 1}
        pages={pages}
        prevDisabled={changingPage || currentPage === 0}
        nextDisabled={changingPage || currentPage + 1 >= pages}
        onPrev={() => loadPage(page.offset - page.limit)}
        onNext={() => loadPage(page.offset + page.limit)}
        summary={`Страница ${currentPage + 1} из ${pages} · Найдено: ${page.total}`}
      />
    </>}
  </section>;
}
