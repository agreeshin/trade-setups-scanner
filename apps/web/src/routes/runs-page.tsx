import { FormEvent, useEffect, useState } from "react";
import { Button } from "../components/ui/button";
import { Input } from "../components/ui/input";
import { PLATFORM_URL, fetchPlatform, type PlatformState } from "../lib/platform-api";

interface RunRow {
  id: number;
  symbol: string;
  status: string;
  cacheHit: boolean;
  setupCount: number;
  error: string | null;
}

export function RunsPage() {
  const [symbols, setSymbols] = useState("");
  const [fromUnix, setFromUnix] = useState("");
  const [toUnix, setToUnix] = useState("");
  const [submitting, setSubmitting] = useState(false);
  const [submitError, setSubmitError] = useState<string | null>(null);
  const [runs, setRuns] = useState<PlatformState<{ runs: RunRow[] }>>({
    kind: "loading",
  });

  const reloadRuns = (): void => {
    void fetchPlatform<{ runs: RunRow[] }>("/v1/replay-runs").then(setRuns);
  };

  useEffect(() => {
    reloadRuns();
  }, []);

  const handleSubmit = async (event: FormEvent<HTMLFormElement>) => {
    event.preventDefault();
    setSubmitError(null);
    const parsed = symbols
      .split(/\r?\n/)
      .map((line) => line.trim())
      .filter((line) => line.length > 0);
    const from = Number(fromUnix);
    const to = Number(toUnix);
    if (parsed.length === 0 || !Number.isFinite(from) || !Number.isFinite(to) || to <= from) {
      setSubmitError("Укажите хотя бы один символ и корректный диапазон Unix-секунд.");
      return;
    }
    setSubmitting(true);
    try {
      const response = await fetch(`${PLATFORM_URL}/v1/replay-runs`, {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({
          instruments: parsed.map((symbol) => ({
            exchange: "bybit",
            category: "linear",
            symbol,
          })),
          fromUnix: from,
          toUnix: to,
        }),
      });
      if (!response.ok) {
        setSubmitError(`Платформа ответила ${response.status}`);
      } else {
        reloadRuns();
      }
    } catch (error) {
      setSubmitError(error instanceof Error ? error.message : "network error");
    } finally {
      setSubmitting(false);
    }
  };

  return (
    <section aria-labelledby="runs-heading">
      <h1 id="runs-heading" className="text-2xl font-semibold">
        Исторические запуски
      </h1>
      <form
        className="mt-4 grid max-w-xl gap-3"
        onSubmit={(event) => {
          void handleSubmit(event);
        }}
      >
        <label className="grid gap-1 text-sm">
          Инструменты (по одному в строке)
          <textarea
            className="min-h-20 rounded-md border border-input bg-background px-3 py-2 text-sm"
            value={symbols}
            onChange={(event) => setSymbols(event.target.value)}
            placeholder={"BTCUSDT\nETHUSDT"}
          />
        </label>
        <div className="grid grid-cols-2 gap-3">
          <label className="grid gap-1 text-sm">
            От (Unix сек)
            <Input value={fromUnix} onChange={(e) => setFromUnix(e.target.value)} />
          </label>
          <label className="grid gap-1 text-sm">
            До (Unix сек)
            <Input value={toUnix} onChange={(e) => setToUnix(e.target.value)} />
          </label>
        </div>
        {submitError !== null && (
          <p role="alert" className="text-sm text-muted-foreground">
            {submitError}
          </p>
        )}
        <Button type="submit" disabled={submitting}>
          {submitting ? "Выполняется…" : "Запустить"}
        </Button>
      </form>

      <h2 className="mt-8 text-lg font-semibold">Последние запуски</h2>
      {runs.kind === "loading" && (
        <p className="mt-2 text-muted-foreground">Загрузка…</p>
      )}
      {runs.kind === "unreachable" && (
        <p role="alert" className="mt-2 text-muted-foreground">
          Платформа недоступна ({runs.detail}).
        </p>
      )}
      {runs.kind === "ready" && runs.data.runs.length === 0 && (
        <p className="mt-2 text-muted-foreground">Запусков пока нет.</p>
      )}
      {runs.kind === "ready" && runs.data.runs.length > 0 && (
        <table className="mt-3 w-full text-sm">
          <thead>
            <tr className="border-b border-input text-left text-muted-foreground">
              <th className="py-2">ID</th>
              <th className="py-2">Инструмент</th>
              <th className="py-2">Статус</th>
              <th className="py-2">Кеш</th>
              <th className="py-2">Сетапов</th>
            </tr>
          </thead>
          <tbody>
            {runs.data.runs.map((run) => (
              <tr key={run.id} className="border-b border-input">
                <td className="py-2">{run.id}</td>
                <td className="py-2">{run.symbol}</td>
                <td className="py-2">{run.status}</td>
                <td className="py-2">{run.cacheHit ? "да" : "нет"}</td>
                <td className="py-2">{run.setupCount}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </section>
  );
}
