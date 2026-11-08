import { useEffect, useState } from "react";
import { Alert } from "../components/ui/alert";
import { Button } from "../components/ui/button";
import { Card } from "../components/ui/card";
import { SectionHeader } from "../components/ui/page-header";
import { fetchPlatform } from "../lib/platform-api";
import { InstrumentPicker, type CatalogInstrument } from "./instrument-picker";
import { errorText, writePlatform } from "./platform-controls";

interface LiveList { configured: boolean; instruments: { exchange: string; category: string; symbol: string }[] }

export function LiveInstruments() {
  const [catalog, setCatalog] = useState<CatalogInstrument[]>([]);
  const [symbols, setSymbols] = useState("");
  const [ready, setReady] = useState(false);
  const [busy, setBusy] = useState(false);
  const [configured, setConfigured] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const [message, setMessage] = useState<string | null>(null);
  useEffect(() => {
    let cancelled = false;
    void Promise.all([fetchPlatform<{ instruments: CatalogInstrument[] }>("/v1/instruments"), fetchPlatform<LiveList>("/v1/instrument-lists/live")]).then(([available, saved]) => {
      if (cancelled) return;
      if (available.kind !== "ready" || saved.kind !== "ready") {
        setError("Live-список или каталог недоступен. Сохранение заблокировано, текущий выбор сервера не изменён."); return;
      }
      const active = available.data.instruments.filter((instrument) => instrument.status === "active" && instrument.exchange === "bybit" && instrument.category === "linear");
      setCatalog(active); setConfigured(saved.data.configured);
      setSymbols((saved.data.configured ? saved.data.instruments : active).map((instrument) => instrument.symbol).join("\n"));
      setReady(true);
    });
    return () => { cancelled = true; };
  }, []);
  async function save() {
    setError(null); setMessage(null);
    const selected = [...new Set(symbols.split(/\s+/).filter(Boolean))];
    if (selected.some((symbol) => !catalog.some((instrument) => instrument.symbol === symbol))) { setError("Уберите отсутствующие или неактивные инструменты перед сохранением."); return; }
    setBusy(true);
    try {
      await writePlatform("/v1/instrument-lists/live", { instruments: selected.map((symbol) => ({ exchange: "bybit", category: "linear", symbol })) });
      setConfigured(true); setMessage("Live-список сохранён. Состояние сканера не изменено.");
    } catch (cause) { setError(errorText(cause)); }
    finally { setBusy(false); }
  }
  return <section className="mt-10 grid max-w-3xl gap-3" aria-labelledby="live-instruments-heading">
    <SectionHeader id="live-instruments-heading" title="Отдельный список live-инструментов" />
    <Card className="grid gap-3">
      {!ready && !error && <p className="text-sm text-muted-foreground">Загрузка live-списка…</p>}
      {ready && <><p className="text-sm text-muted-foreground">{configured ? "Загружен сохранённый выбор." : "Список ещё не настроен: используются все активные инструменты каталога. Сохраните, чтобы зафиксировать выбор."}</p><InstrumentPicker live catalog={catalog} symbols={symbols} onChange={(value) => { setSymbols(value); setMessage(null); }} disabled={busy} /></>}
      {error && <Alert variant="destructive" role="alert">{error}</Alert>}
      {message && <Alert variant="success" role="status">{message}</Alert>}
      <div><Button disabled={!ready || busy} onClick={() => { void save(); }}>Сохранить live-список</Button></div>
    </Card>
  </section>;
}
