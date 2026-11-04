import { useEffect, useState } from "react";
import { apiFetch, fetchPlatform, type PlatformState } from "../lib/platform-api";
import { Button } from "../components/ui/button";
import { errorText, writePlatform } from "./platform-controls";

export function OperationSettings() {
  const [state, setState] = useState<PlatformState<{ paused: boolean }>>({ kind: "loading" });
  const [error, setError] = useState<string | null>(null);
  const [busy, setBusy] = useState(false);
  useEffect(() => {
    let cancelled = false;
    void fetchPlatform<{ paused: boolean }>("/v1/settings/replay").then((value) => { if (!cancelled) setState(value); });
    return () => { cancelled = true; };
  }, []);
  async function toggle() {
    if (state.kind !== "ready") return;
    setBusy(true); setError(null);
    try {
      await writePlatform("/v1/settings/replay", { paused: !state.data.paused });
      setState(await fetchPlatform<{ paused: boolean }>("/v1/settings/replay"));
    } catch (cause) { setError(errorText(cause)); }
    finally { setBusy(false); }
  }
  async function download() {
    setBusy(true); setError(null);
    try {
      const response = await apiFetch("/v1/export/config.json");
      if (!response.ok) throw new Error(`HTTP ${response.status}`);
      const url = URL.createObjectURL(await response.blob());
      const anchor = document.createElement("a");
      anchor.href = url; anchor.download = "platform-config.json";
      document.body.append(anchor); anchor.click(); anchor.remove();
      URL.revokeObjectURL(url);
    } catch (cause) { setError(errorText(cause)); }
    finally { setBusy(false); }
  }
  return <section aria-labelledby="operations-heading" className="mt-8 grid max-w-3xl gap-3">
    <h2 id="operations-heading" className="text-xl font-semibold">Выполнение и конфигурация</h2>
    {state.kind === "loading" && <p>Загрузка состояния очереди…</p>}
    {state.kind === "unreachable" && <p role="alert">Очередь недоступна ({state.detail})</p>}
    {state.kind === "ready" && <><p>{state.data.paused ? "Исторические расчёты приостановлены" : "Очередь исторических расчётов работает"}</p><Button variant="outline" disabled={busy} onClick={() => { void toggle(); }}>{state.data.paused ? "Продолжить расчёты" : "Приостановить расчёты"}</Button></>}
    <p className="text-sm text-muted-foreground">Пауза сохраняется в БД; выполняющийся расчёт остановится между страницами. Уже рассчитанные результаты не удаляются. При перезапуске сервиса прерванные расчёты отмечаются ошибкой и требуют нового запуска.</p>
    <Button variant="outline" disabled={busy} onClick={() => { void download(); }}>Экспорт конфигурации без секретов</Button>
    <p className="text-sm text-muted-foreground">Экспорт включает параметры алгоритма, списки инструментов и режимы работы. Это не резервная копия БД: история свечей и данные платформы резервируются отдельно администратором.</p>
    {error && <p role="alert">{error}</p>}
  </section>;
}
