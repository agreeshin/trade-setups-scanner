import { useState, type FormEvent } from "react";
import { Button } from "../components/ui/button";
import { Input } from "../components/ui/input";
import { apiFetch } from "../lib/platform-api";
import { errorText } from "./platform-controls";

type Scope = "platform" | "market-data";

export function BackupSettings() {
  const [scope, setScope] = useState<Scope>("platform");
  const [file, setFile] = useState<File | null>(null);
  const [confirmation, setConfirmation] = useState("");
  const [message, setMessage] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  const expected = `RESTORE ${scope}`;

  async function stage(event: FormEvent) {
    event.preventDefault();
    if (!file || confirmation !== expected) return;
    setBusy(true); setMessage(null);
    try {
      const path = scope === "platform" ? "/v1/admin/restore" : "/v1/admin/market-restore";
      const response = await apiFetch(path, { method: "POST", headers: {
        "content-type": "application/octet-stream", "x-restore-confirmation": confirmation,
      }, body: file });
      const body = await response.json() as { error?: string };
      if (!response.ok) throw new Error(body.error ?? `HTTP ${response.status}`);
      setMessage("Копия проверена и подготовлена. Она будет применена при контролируемом restart сервиса; текущая БД сначала сохранится для rollback.");
      setConfirmation(""); setFile(null);
    } catch (error) { setMessage(errorText(error)); }
    finally { setBusy(false); }
  }

  return <section className="mt-8 grid max-w-xl gap-3" aria-labelledby="backup-heading">
    <h2 id="backup-heading" className="text-xl font-semibold">Резервные копии и восстановление</h2>
    <p className="text-sm text-muted-foreground">Копии двух БД независимы. Platform backup содержит чувствительные Web Push endpoints и хранится защищённо.</p>
    <div className="flex flex-wrap gap-2"><Button asChild variant="outline"><a href="/v1/admin/backup" download>Скачать platform DB</a></Button><Button asChild variant="outline"><a href="/v1/admin/market-backup" download>Скачать market-data DB</a></Button></div>
    <form className="grid gap-3" onSubmit={(event) => { void stage(event); }}>
      <label className="grid gap-1 text-sm">Область<select className="h-10 rounded-md border bg-background px-3" value={scope} onChange={(event) => { setScope(event.target.value as Scope); setConfirmation(""); }}><option value="platform">Platform DB</option><option value="market-data">Market-data DB</option></select></label>
      <label className="grid gap-1 text-sm">Файл backup<Input type="file" accept=".tsb,application/octet-stream" onChange={(event) => setFile(event.target.files?.[0] ?? null)} /></label>
      <label className="grid gap-1 text-sm">Введите <code>{expected}</code><Input value={confirmation} autoComplete="off" onChange={(event) => setConfirmation(event.target.value)} /></label>
      <p className="text-xs text-muted-foreground">Upload проверяет scope, checksum, совместимость и место. Автоматического restart нет.</p>
      {message && <p role="status">{message}</p>}
      <Button type="submit" disabled={busy || !file || confirmation !== expected}>{busy ? "Проверка…" : "Проверить и подготовить restore"}</Button>
    </form>
  </section>;
}
