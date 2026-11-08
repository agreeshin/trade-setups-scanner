import { useState } from "react";
import { Alert } from "../components/ui/alert";
import { Button } from "../components/ui/button";
import { Checkbox } from "../components/ui/checkbox";
import { Field } from "../components/ui/field";
import { Input } from "../components/ui/input";
import { Textarea } from "../components/ui/textarea";

export interface CatalogInstrument { id: number; exchange: string; category: string; symbol: string; status: string }
export interface ImportPreview { accepted: string[]; excluded: string[]; duplicates: number }

export function previewSymbols(text: string, catalog: CatalogInstrument[]): ImportPreview {
  const available = new Set(catalog.map((instrument) => instrument.symbol));
  const accepted = new Set<string>();
  const excluded: string[] = [];
  let duplicates = 0;
  for (const raw of text.replace(/^\uFEFF/, "").split(/\r?\n/)) {
    const line = raw.trim();
    if (!line) continue;
    const symbol = (/^"[^"\r\n]+"$/.test(line) ? line.slice(1, -1) : line).toUpperCase();
    if (!/^[A-Z0-9]{2,40}$/.test(symbol) || !available.has(symbol)) excluded.push(line);
    else if (accepted.has(symbol)) duplicates += 1;
    else accepted.add(symbol);
  }
  return { accepted: [...accepted], excluded, duplicates };
}

export function InstrumentPicker({ catalog, symbols, onChange, disabled, live = false, limit = 100 }: { catalog: CatalogInstrument[]; symbols: string; onChange: (symbols: string) => void; disabled: boolean; live?: boolean; limit?: number | null }) {
  const [search, setSearch] = useState("");
  const [csv, setCsv] = useState("");
  const [preview, setPreview] = useState<ImportPreview | null>(null);
  const [error, setError] = useState<string | null>(null);
  const selected = new Set(symbols.split(/[\s,;]+/).filter(Boolean));
  const visible = catalog.filter((instrument) => instrument.symbol.toLowerCase().includes(search.trim().toLowerCase()));
  const unavailable = [...selected].filter((symbol) => !catalog.some((instrument) => instrument.symbol === symbol));
  function toggle(symbol: string) {
    if (selected.has(symbol)) selected.delete(symbol); else selected.add(symbol);
    onChange([...selected].join("\n"));
  }
  async function readFile(file: File | undefined) {
    setError(null); setPreview(null);
    if (!file) return;
    if (file.size > 262144) { setError("CSV должен быть не больше 256 КиБ."); return; }
    try {
      const text = await file.text();
      setCsv(text); setPreview(previewSymbols(text, catalog));
    } catch { setError("Не удалось прочитать CSV-файл."); }
  }
  return <fieldset className="grid gap-3 rounded-md border border-input p-4" disabled={disabled}>
    <legend className="font-medium">{live ? "Live-инструменты" : "Инструменты из БД"} · Bybit Linear</legend>
    <p className="text-sm text-muted-foreground">{live ? "Только активные инструменты каталога. Этот список независим от исторических запусков; пустой сохранённый выбор означает отсутствие инструментов для live." : "По умолчанию все инструменты каталога БД, включая делистнутые. Наличие инструмента не гарантирует полноту истории выбранного периода."}</p>
    <div className="flex flex-wrap gap-2"><Button type="button" variant="outline" size="sm" onClick={() => onChange(catalog.map((instrument) => instrument.symbol).join("\n"))}>{live ? "Все активные" : "Все из БД"}</Button><Button type="button" variant="outline" size="sm" onClick={() => onChange("")}>Убрать все</Button></div>
    <Field label="Поиск инструмента">{(control) => <Input value={search} onChange={(event) => setSearch(event.target.value)} {...control} />}</Field>
    <div className="grid max-h-64 gap-2 overflow-y-auto sm:grid-cols-2">
      {visible.slice(0, 200).map((instrument) => <label key={instrument.id} className="flex items-center gap-2 text-sm"><Checkbox checked={selected.has(instrument.symbol)} onCheckedChange={() => toggle(instrument.symbol)} /><span>{instrument.symbol} <span className="text-muted-foreground">({instrument.status})</span></span></label>)}
    </div>
    {visible.length > 200 && <p className="text-sm text-muted-foreground">Показаны первые 200 совпадений. Уточните поиск.</p>}
    <p className="text-sm" aria-live="polite">Выбрано: {selected.size} из {catalog.length}.{!live && limit !== null && ` Лимит одного запуска: ${limit}.`}</p>
    {!live && limit !== null && selected.size > limit && <Alert variant="warning" role="alert">Уменьшите список до {limit} инструментов перед запуском. Список не усечён.</Alert>}
    {unavailable.length > 0 && <div className="grid gap-2"><Alert variant="warning" role="alert">Сохранённые символы отсутствуют в текущем каталоге: {unavailable.join(", ")}</Alert><div><Button type="button" variant="outline" size="sm" onClick={() => onChange([...selected].filter((symbol) => !unavailable.includes(symbol)).join("\n"))}>Убрать отсутствующие</Button></div></div>}
    <Field label="Импорт CSV без заголовка">{(control) => <Input type="file" accept=".csv,text/csv,text/plain" onChange={(event) => { void readFile(event.target.files?.[0]); }} {...control} />}</Field>
    <Field label="CSV: один символ на строку">{(control) => <Textarea maxLength={262144} value={csv} placeholder={"BTCUSDT\nETHUSDT"} onChange={(event) => { setCsv(event.target.value); setPreview(null); }} {...control} />}</Field>
    <div><Button type="button" variant="outline" size="sm" onClick={() => { setError(null); setPreview(previewSymbols(csv, catalog)); }}>Проверить импорт</Button></div>
    {error && <Alert variant="destructive" role="alert">{error}</Alert>}
    {preview && <div className="grid gap-2 rounded-md bg-muted/40 p-3 text-sm"><p role="status">Принято: {preview.accepted.length}. Дубли: {preview.duplicates}. Исключено: {preview.excluded.length}.</p>
      <p>Будет выбран список: {preview.accepted.slice(0, 100).join(", ") || "пусто"}{preview.accepted.length > 100 ? "… (показаны первые 100)" : ""}</p>
      {preview.excluded.length > 0 && <p className="break-all">Неизвестные символы или неверные строки: {preview.excluded.slice(0, 50).map((line) => line.slice(0, 80)).join(", ")}{preview.excluded.length > 50 ? "… (показаны первые 50)" : ""}</p>}
      <div><Button type="button" size="sm" disabled={preview.accepted.length === 0} onClick={() => { onChange(preview.accepted.join("\n")); setPreview(null); }}>Заменить выбор принятым списком</Button></div>
    </div>}
  </fieldset>;
}
