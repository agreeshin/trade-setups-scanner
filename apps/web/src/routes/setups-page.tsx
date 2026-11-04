import { useEffect, useState } from "react";
import { Button } from "../components/ui/button";
import { Input } from "../components/ui/input";
import { apiFetch, fetchPlatform, type PlatformState } from "../lib/platform-api";
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
  async function exportCsv() {
    setBusy(true); setError(null); setMessage(null);
    try {
      const response = await apiFetch("/v1/export/setups.csv");
      if (!response.ok) throw new Error(`Экспорт недоступен: HTTP ${response.status}`);
      const blob = await response.blob();
      const url = URL.createObjectURL(blob);
      const link = document.createElement("a");
      link.href = url; link.download = "live-setups.csv"; document.body.append(link); link.click(); link.remove();
      window.setTimeout(() => URL.revokeObjectURL(url), 1000);
      setMessage("CSV подготовлен. Экспорт содержит до 50 000 live-сетапов, без фильтра экрана.");
    } catch (cause) { setError(errorText(cause)); }
    finally { setBusy(false); }
  }
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
  return <section aria-labelledby="setups-heading">
    <h1 id="setups-heading" className="text-2xl font-semibold">Сетапы</h1>
    <p className="mt-2 text-sm text-muted-foreground">Поиск по всей сохранённой live-ленте. Загрузка по 25 записей; новые поступления появятся после «Обновить» или изменения фильтра.</p>
    <div className="mt-4 flex flex-wrap items-end gap-3">
      <label className="grid gap-1 text-sm">Инструмент, биржа или алгоритм<Input disabled={!preferencesReady} value={query.filter} maxLength={200} onChange={(event) => { setQuery({ filter: event.target.value, offset: 0 }); setState({ kind: "loading" }); setMessage(null); }} /></label>
      <Button variant="outline" disabled={busy || !preferencesReady} onClick={() => { void saveFilter(); }}>Сохранить фильтр</Button>
      <Button variant="outline" disabled={!preferencesReady} onClick={() => { setQuery({ filter: query.filter, offset: 0 }); setState({ kind: "loading" }); }}>Обновить</Button>
      <Button disabled={busy} onClick={() => { void exportCsv(); }}>Экспорт CSV (без фильтра)</Button>
    </div>
    {error && <p role="alert" className="mt-3">{error}</p>}{message && <p role="status" className="mt-3">{message}</p>}
    {state.kind === "loading" && <p role="status">Загрузка…</p>}
    {state.kind === "unreachable" && <p role="alert">Лента недоступна ({state.detail})</p>}
    {state.kind === "ready" && <><SetupTable setups={state.data.setups} /><nav aria-label="Страницы сетапов" className="mt-4 flex flex-wrap items-center gap-3"><Button variant="outline" disabled={currentPage === 0} onClick={() => loadPage(state.data.offset - state.data.limit)}>Назад</Button><span aria-live="polite">Страница {currentPage + 1} из {pages} · Найдено: {state.data.total}</span><Button variant="outline" disabled={currentPage + 1 >= pages} onClick={() => loadPage(state.data.offset + state.data.limit)}>Далее</Button></nav></>}
  </section>;
}
