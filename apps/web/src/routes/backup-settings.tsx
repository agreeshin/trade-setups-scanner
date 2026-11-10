import { useState, type FormEvent } from "react";
import { Alert } from "../components/ui/alert";
import { Button } from "../components/ui/button";
import { Card } from "../components/ui/card";
import { Field } from "../components/ui/field";
import { Input } from "../components/ui/input";
import { SectionHeader } from "../components/ui/page-header";
import { Select } from "../components/ui/select";
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

  return <section className="mt-10 grid gap-3" aria-labelledby="backup-heading">
    <SectionHeader id="backup-heading" title="Резервные копии и восстановление" />
    <Card className="grid gap-3">
      <p className="text-sm text-muted-foreground">Копии двух БД независимы. Platform backup содержит чувствительные Web Push endpoints и хранится защищённо.</p>
      <div className="flex flex-wrap gap-2"><Button asChild variant="outline"><a href="/v1/admin/backup" download>Скачать platform DB</a></Button><Button asChild variant="outline"><a href="/v1/admin/market-backup" download>Скачать market-data DB</a></Button></div>
      <form className="grid gap-4 border-t border-input pt-4" onSubmit={(event) => { void stage(event); }}>
        <Field label="Область">{(control) => <Select value={scope} onChange={(event) => { setScope(event.target.value as Scope); setConfirmation(""); }} {...control}><option value="platform">Platform DB</option><option value="market-data">Market-data DB</option></Select>}</Field>
        <Field label="Файл backup">{(control) => <Input type="file" accept=".tsb,application/octet-stream" onChange={(event) => setFile(event.target.files?.[0] ?? null)} {...control} />}</Field>
        <Field label={<>Введите <code className="text-xs">{expected}</code></>}>{(control) => <Input value={confirmation} autoComplete="off" onChange={(event) => setConfirmation(event.target.value)} {...control} />}</Field>
        <p className="text-xs text-muted-foreground">Upload проверяет scope, checksum, совместимость и место. Автоматического restart нет.</p>
        {message && <Alert variant="info" role="status">{message}</Alert>}
        <div><Button type="submit" disabled={busy || !file || confirmation !== expected}>{busy ? "Проверка…" : "Проверить и подготовить restore"}</Button></div>
      </form>
    </Card>
  </section>;
}
