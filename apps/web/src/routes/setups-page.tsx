import { useEffect, useState } from "react";
import { Alert, AlertDescription } from "../components/ui/alert";
import { Button } from "../components/ui/button";
import { Card, CardContent } from "../components/ui/card";
import { Field } from "../components/ui/field";
import { Input } from "../components/ui/input";
import { PageHeader } from "../components/ui/page-header";
import { Pagination } from "../components/ui/pagination";
import { Spinner } from "../components/ui/spinner";
import { PLATFORM_URL, fetchPlatform, type PlatformState } from "../lib/platform-api";
import { errorText, savePreference, type SetupRow } from "./platform-controls";
import { SetupTable } from "./setup-table";

interface SetupPage { setups: SetupRow[]; offset: number; limit: number; total: number; snapshotId: number }

export function SetupsPage() {
  const [state, setState] = useState<PlatformState<SetupPage>>({ kind: "loading" });
  const [query, setQuery] = useState<{ filter: string; offset: number; snapshotId?: number }>({ filter: "", offset: 0 });
  const [preferencesReady, setPreferencesReady] = useState(false);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [message, setMessage] = useState<string | null>(null);
  useEffect(() => {
    if (!preferencesReady) return;
    const controller = new AbortController();
    const params = new URLSearchParams({ offset: String(query.offset), limit: "25", q: query.filter });
    if (query.snapshotId !== undefined) params.set("snapshotId", String(query.snapshotId));
    void fetchPlatform<SetupPage>(`/v1/setups?${params}`, controller.signal).then((result) => {
      if (!controller.signal.aborted) setState(result);
    });
    return () => { controller.abort(); };
  }, [query, preferencesReady]);
  useEffect(() => {
    let cancelled = false;
    void fetchPlatform<{ value: { setupFilter?: string } }>("/v1/settings/ui").then((result) => {
      if (cancelled) return;
      if (result.kind === "ready" && typeof result.data.value.setupFilter === "string") setQuery({ filter: result.data.value.setupFilter.slice(0, 200), offset: 0 });
      else if (result.kind === "unreachable") setError(`Сохранённый фильтр недоступен (${result.detail})`);
      setPreferencesReady(true);
    });
    return () => { cancelled = true; };
  }, []);
  async function saveFilter() {
    setBusy(true); setError(null); setMessage(null);
    try { await savePreference("setupFilter", query.filter); setMessage("Фильтр сохранён в базе сигналов."); }
    catch (cause) { setError(errorText(cause)); }
    finally { setBusy(false); }
  }
  function loadPage(offset: number) {
    if (state.kind !== "ready") return;
    setQuery({ ...query, offset, snapshotId: state.data.snapshotId });
    setState({ kind: "loading" });
  }
  const pages = state.kind === "ready" ? Math.max(1, Math.ceil(state.data.total / state.data.limit)) : 1;
  const currentPage = state.kind === "ready" ? Math.floor(state.data.offset / state.data.limit) : 0;
  return <section aria-labelledby="setups-heading" className="grid gap-4">
    <PageHeader id="setups-heading" title="Сетапы" description="Поиск по всей сохранённой live-ленте. Загрузка по 25 записей; новые поступления появятся после «Обновить» или изменения фильтра." />
    <Card>
      <CardContent className="flex flex-wrap items-end gap-3">
        <Field label="Инструмент, биржа или алгоритм" className="w-full max-w-sm">
          {(control) => <Input {...control} disabled={!preferencesReady} value={query.filter} maxLength={200} onChange={(event) => { setQuery({ filter: event.target.value, offset: 0 }); setState({ kind: "loading" }); setMessage(null); }} />}
        </Field>
        <div className="flex flex-wrap gap-2">
          <Button variant="outline" disabled={busy || !preferencesReady} onClick={() => { void saveFilter(); }}>Сохранить фильтр</Button>
          <Button variant="outline" disabled={!preferencesReady} onClick={() => { setQuery({ filter: query.filter, offset: 0 }); setState({ kind: "loading" }); }}>Обновить</Button>
          <Button asChild variant="outline"><a href={`${PLATFORM_URL}/v1/export/setups.csv`}>Экспорт CSV (без фильтра)</a></Button>
        </div>
        <p className="w-full text-xs text-muted-foreground">CSV содержит всю сохранённую live-ленту на начало выгрузки, без фильтра экрана и лимита строк. Статус и отмена доступны в загрузках браузера.</p>
      </CardContent>
    </Card>
    {error && <Alert variant="destructive" role="alert"><AlertDescription>{error}</AlertDescription></Alert>}
    {message && <Alert variant="success" role="status"><AlertDescription>{message}</AlertDescription></Alert>}
    {state.kind === "loading" && <Spinner label="Загрузка…" />}
    {state.kind === "unreachable" && <Alert variant="destructive" role="alert">Лента недоступна ({state.detail})</Alert>}
    {state.kind === "ready" && <>
      <SetupTable setups={state.data.setups} />
      <Pagination
        label="Страницы сетапов"
        page={currentPage + 1}
        pages={pages}
        prevDisabled={currentPage === 0}
        nextDisabled={currentPage + 1 >= pages}
        onPrev={() => loadPage(state.data.offset - state.data.limit)}
        onNext={() => loadPage(state.data.offset + state.data.limit)}
        summary={`Страница ${currentPage + 1} из ${pages} · Найдено: ${state.data.total}`}
      />
    </>}
  </section>;
}
