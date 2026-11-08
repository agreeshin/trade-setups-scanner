import { Alert } from "../components/ui/alert";
import { Card } from "../components/ui/card";
import { localTime, localTimezone } from "./platform-controls";
import { qualityLabel, type ReplayQuality } from "./replay-quality";

export interface ReplayInstrument { exchange: string; category: string; symbol: string }
export interface PreflightResponse {
  checkedAt: string; algorithmVersion: string; requiredHistoryMinutes: number; canRun: boolean;
  request: { instruments: ReplayInstrument[]; fromUnix: number; toUnix: number; algorithmId: string; algorithmVersion: string; params: Record<string, string | number>; async: true };
  results: (ReplayInstrument & {
    cacheHit: boolean; status: "cached" | "ready" | "unavailable" | "not_found";
    computedAt?: string; quality?: ReplayQuality | null; setupCount?: number; error?: string;
    coverage?: { instrumentId: number; fromUnix: number; toUnix: number; splitUnix: number; expectedMinutes: number; presentMinutes: number; missingMinutes: number; warmupExpectedMinutes: number; warmupPresentMinutes: number; signalExpectedMinutes: number; signalPresentMinutes: number; gaps: { fromUnix: number; toUnix: number }[]; gapCount: number; offset: number; limit: number; checkedAt: string };
  })[];
}

export function ReplayPreflight({ preview }: { preview: PreflightResponse }) {
  return <Card asChild><section aria-label="Предварительная проверка" className="grid gap-3">
    <h2 className="text-lg font-semibold">Предварительная проверка данных</h2>
    <p className="text-sm">Проверено: {new Date(preview.checkedAt).toLocaleString("ru-RU")}. Версия алгоритма: {preview.algorithmVersion}. Требуемая история: {preview.requiredHistoryMinutes} мин.</p>
    <p className="text-sm">Период расчёта: {localTime(preview.request.fromUnix)} – {localTime(preview.request.toUnix)} ({localTimezone}).</p>
    <details><summary className="cursor-pointer">Снимок параметров для подтверждения</summary><dl className="mt-2 grid gap-2 text-sm sm:grid-cols-2">{Object.entries(preview.request.params).map(([key, value]) => <div key={key}><dt className="break-words text-muted-foreground">{key}</dt><dd>{value}</dd></div>)}</dl></details>
    <p className="text-sm text-muted-foreground">Это снимок на момент проверки. Кеш и покрытие могут измениться до исполнения; исполнитель повторно проверяет кеш. Покрытие минут не гарантирует пригодность по допускам алгоритма. Частичный расчёт по имеющимся данным разрешён; история автоматически не догружается.</p>
    {preview.results.map((item) => <article key={`${item.exchange}/${item.category}/${item.symbol}`} className="grid gap-2 rounded-md border border-input/80 bg-background/40 p-3 text-sm">
      <h3 className="font-semibold">{item.exchange} / {item.category} / {item.symbol}</h3>
      {item.status === "cached" ? <>
        <p>Из кеша · Сетапов: {item.setupCount ?? "не сообщено"}</p>
        <p>Исходное вычисление: {item.computedAt ? new Date(item.computedAt).toLocaleString("ru-RU") : "время не сохранено"}. Качество: {qualityLabel(item.quality)}.</p>
        <p className="text-muted-foreground">Покрытие истории повторно не запрашивалось. Это исходное качество кеша, а не текущая проверка данных.</p>
      </> : item.status === "ready" && item.coverage ? <>
        <p>Покрытие получено: {new Date(item.coverage.checkedAt).toLocaleString("ru-RU")}. Имеется {item.coverage.presentMinutes} из {item.coverage.expectedMinutes} минут; отсутствует {item.coverage.missingMinutes}.</p>
        <p>Прогрев: {item.coverage.warmupPresentMinutes} из {item.coverage.warmupExpectedMinutes} минут. Период расчёта: {item.coverage.signalPresentMinutes} из {item.coverage.signalExpectedMinutes} минут.</p>
        <p className="text-muted-foreground">Входной диапазон: {localTime(item.coverage.fromUnix)} – {localTime(item.coverage.toUnix)}; граница прогрева: {localTime(item.coverage.splitUnix)}.</p>
        {item.coverage.presentMinutes === 0 && <p>Проверено: в этом входном диапазоне истории нет. Это не ошибка доступности; пригодных минут может не оказаться.</p>}
        {item.coverage.missingMinutes > 0 && <p>Есть отсутствующие минуты. При подтверждении будет выполнен расчёт по имеющимся данным; итог может быть частичным или без пригодных минут.</p>}
        <details><summary className="cursor-pointer">Пропуски: показано {Math.min(50, item.coverage.gaps.length)} из {item.coverage.gapCount} интервалов</summary>
          <p className="mt-2 text-xs text-muted-foreground">Ограниченный предварительный список. Интервалы [начало, конец), на UTC-сетке открытия минут; показаны в {localTimezone}.</p>
          <ul className="mt-2 max-h-64 overflow-y-auto">{item.coverage.gaps.slice(0, 50).map((gap) => <li key={`${gap.fromUnix}-${gap.toUnix}`}>{localTime(gap.fromUnix)} – {localTime(gap.toUnix)}</li>)}</ul>
        </details>
      </> : <Alert variant="warning" role="alert">{item.status === "not_found" ? "Инструмент не найден" : "Покрытие недоступно или не получено"}{item.error ? `: ${item.error}` : ". Это не означает пустую историю."}</Alert>}
    </article>)}
    {!preview.canRun && <Alert variant="destructive" role="alert">Запуск недоступен: устраните ошибки проверки и проверьте данные снова.</Alert>}
  </section></Card>;
}
