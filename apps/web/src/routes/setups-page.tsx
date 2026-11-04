import { useEffect, useState } from "react";
import { Button } from "../components/ui/button";
import { Input } from "../components/ui/input";
import { apiFetch, fetchPlatform, type PlatformState } from "../lib/platform-api";
import { errorText, savePreference, type SetupRow } from "./platform-controls";
import { SetupTable } from "./setup-table";

export function SetupsPage() {
  const [state, setState] = useState<PlatformState<{ setups: SetupRow[] }>>({ kind: "loading" });
  const [filter, setFilter] = useState("");
  const [preferencesReady, setPreferencesReady] = useState(false);
  const [page, setPage] = useState(0);
  const [reload, setReload] = useState(0);
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [message, setMessage] = useState<string | null>(null);
  useEffect(() => {
    let cancelled = false;
    void fetchPlatform<{ setups: SetupRow[] }>("/v1/setups").then((result) => { if (!cancelled) setState(result); });
    return () => { cancelled = true; };
  }, [reload]);
  useEffect(() => {
    let cancelled = false;
    void fetchPlatform<{ value: { setupFilter?: string } }>("/v1/settings/ui").then((result) => {
      if (cancelled) return;
      if (result.kind === "ready" && typeof result.data.value.setupFilter === "string") setFilter(result.data.value.setupFilter);
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
    try { await savePreference("setupFilter", filter); setMessage("Фильтр сохранён в базе сигналов."); }
    catch (cause) { setError(errorText(cause)); }
    finally { setBusy(false); }
  }
  const filtered = state.kind === "ready" ? state.data.setups.filter((row) => `${row.symbol} ${row.exchange} ${row.algorithmId}`.toLowerCase().includes(filter.trim().toLowerCase())) : [];
  const pages = Math.max(1, Math.ceil(filtered.length / 25));
  const currentPage = Math.min(page, pages - 1);
  return <section aria-labelledby="setups-heading">
    <h1 id="setups-heading" className="text-2xl font-semibold">Сетапы</h1>
    <p className="mt-2 text-sm text-muted-foreground">Показаны до 500 последних live-сетапов. Поиск и страницы работают только внутри этой выборки.</p>
    <div className="mt-4 flex flex-wrap items-end gap-3">
      <label className="grid gap-1 text-sm">Инструмент, биржа или алгоритм<Input disabled={!preferencesReady} value={filter} maxLength={100} onChange={(event) => { setFilter(event.target.value); setPage(0); setMessage(null); }} /></label>
      <Button variant="outline" disabled={busy || !preferencesReady} onClick={() => { void saveFilter(); }}>Сохранить фильтр</Button>
      <Button variant="outline" onClick={() => setReload((value) => value + 1)}>Обновить</Button>
      <Button disabled={busy} onClick={() => { void exportCsv(); }}>Экспорт CSV (без фильтра)</Button>
    </div>
    {error && <p role="alert" className="mt-3">{error}</p>}{message && <p role="status" className="mt-3">{message}</p>}
    {state.kind === "loading" && <p>Загрузка…</p>}
    {state.kind === "unreachable" && <p role="alert">Лента недоступна ({state.detail})</p>}
    {state.kind === "ready" && <><SetupTable setups={filtered.slice(currentPage * 25, (currentPage + 1) * 25)} /><nav aria-label="Страницы сетапов" className="mt-4 flex flex-wrap items-center gap-3"><Button variant="outline" disabled={currentPage === 0} onClick={() => setPage(currentPage - 1)}>Назад</Button><span aria-live="polite">Страница {currentPage + 1} из {pages} · Найдено: {filtered.length}</span><Button variant="outline" disabled={currentPage + 1 >= pages} onClick={() => setPage(currentPage + 1)}>Далее</Button></nav></>}
  </section>;
}
