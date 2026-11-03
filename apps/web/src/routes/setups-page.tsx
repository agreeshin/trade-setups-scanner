import { useEffect, useState } from "react";
import { fetchPlatform, type PlatformState } from "../lib/platform-api";

interface SetupRow {
  exchange: string;
  symbol: string;
  algorithmId: string;
  openTimeUnix: number;
  closeTimeUnix: number;
  closePrice: string;
}

export function SetupsPage() {
  const [state, setState] = useState<PlatformState<{ setups: SetupRow[] }>>({
    kind: "loading",
  });

  useEffect(() => {
    let cancelled = false;
    void fetchPlatform<{ setups: SetupRow[] }>("/v1/setups").then((result) => {
      if (!cancelled) {
        setState(result);
      }
    });
    return () => {
      cancelled = true;
    };
  }, []);

  return (
    <section aria-labelledby="setups-heading">
      <h1 id="setups-heading" className="text-2xl font-semibold">
        Сетапы
      </h1>
      {state.kind === "loading" && (
        <p className="mt-2 text-muted-foreground">Загрузка…</p>
      )}
      {state.kind === "unreachable" && (
        <p role="alert" className="mt-2 text-muted-foreground">
          Платформа сетапов недоступна ({state.detail}). Свежие сетапы появятся
          после восстановления соединения; это не означает отсутствие сетапов.
        </p>
      )}
      {state.kind === "ready" && state.data.setups.length === 0 && (
        <p className="mt-2 text-muted-foreground">
          Пока нет сохранённых live-сетапов.
        </p>
      )}
      {state.kind === "ready" && state.data.setups.length > 0 && (
        <table className="mt-4 w-full text-sm">
          <thead>
            <tr className="border-b border-input text-left text-muted-foreground">
              <th className="py-2">Инструмент</th>
              <th className="py-2">Время (Unix)</th>
              <th className="py-2">Цена close</th>
              <th className="py-2">Алгоритм</th>
            </tr>
          </thead>
          <tbody>
            {state.data.setups.map((setup) => (
              <tr key={`${setup.symbol}-${setup.closeTimeUnix}`} className="border-b border-input">
                <td className="py-2">{setup.symbol}</td>
                <td className="py-2">{setup.closeTimeUnix}</td>
                <td className="py-2">{setup.closePrice}</td>
                <td className="py-2">{setup.algorithmId}</td>
              </tr>
            ))}
          </tbody>
        </table>
      )}
    </section>
  );
}
