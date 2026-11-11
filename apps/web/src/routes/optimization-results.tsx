import { Alert } from "../components/ui/alert";
import { Badge } from "../components/ui/badge";
import { Button } from "../components/ui/button";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "../components/ui/table";
import { formatIsoDateTime, formatUnix } from "../lib/datetime-format";
import { isActiveStudy, type OptimizationStudy } from "../lib/optimization-api";
import { parameterLabels } from "../lib/parameter-labels";
import type { Algorithm } from "./platform-controls";
import type { OptimizationController } from "./use-optimization-studies";

const statusLabels: Record<OptimizationStudy["status"], string> = {
  queued: "В очереди", running: "Выполняется", cancel_requested: "Отмена запрошена",
  completed: "Завершено", cancelled: "Отменено", failed: "Ошибка исследования",
};
const phaseLabels: Record<string, string> = { acquiring: "Подготовка снимка данных", initial: "Начальное исследование", sensitivity: "Чувствительность", refinement: "Уточнение", cleanup: "Очистка временных данных" };
const reasonLabels = { trial_budget: "Исчерпан бюджет trials", time_budget: "Исчерпан бюджет времени", space_exhausted: "Конечное пространство исчерпано", cancelled: "Явная остановка" };
// Match the existing Analytics controls, not the optimizer search-coordinate wording.
const analyticsLabels: Record<string, string> = {
  clusterEnabled: "Кластеризовать сетапы", clusterGapMinutes: "Разрыв кластера, ч",
  timeShiftMinutes: "Time-shift, мин", priceChangePct: "Смещение цены, %",
  setupMaturity: "Зрелость сетапа в кластере", horizonMinutes: "Горизонт сделки, мин (фиксирован)",
};

export function OptimizationResults({ study, algorithm, controller, onNew }: { study: OptimizationStudy; algorithm: Algorithm; controller: OptimizationController; onNew: () => void }) {
  const { progress, spec, result } = study;
  const best = result?.best;
  const frequencyThreshold = spec.minSetupsPerDay === undefined ? "Гейт не применялся"
    : spec.minSetupsPerDay === 0 ? "0 (гейт отключён)" : String(spec.minSetupsPerDay);
  return <section className="grid min-w-0 gap-4" aria-label="Статус исследования">
    <div className="flex flex-wrap items-center gap-2">
      <h3 className="font-semibold">Исследование #{study.id}</h3>
      <Badge variant={study.status === "failed" ? "danger" : study.status === "completed" ? "success" : "info"}>{statusLabels[study.status]}</Badge>
    </div>
    <p className="text-sm" role="status">{phaseLabels[progress.phase] ?? progress.phase}</p>
    <dl className="grid gap-2 text-sm sm:grid-cols-2">
      <div><dt className="text-muted-foreground">Завершено trials / бюджет</dt><dd>{progress.evaluatedCandidates} / {progress.maxTrials}</dd></div>
      <div><dt className="text-muted-foreground">Оценимых кандидатов</dt><dd>{progress.scorableCandidates}</dd></div>
      <div><dt className="text-muted-foreground">Прошло, секунд</dt><dd>{progress.elapsedSeconds}</dd></div>
      <div><dt className="text-muted-foreground">Текущий лучший score</dt><dd>{progress.bestScore === null ? "Ещё нет оценимого кандидата" : `${progress.bestScore} R`}</dd></div>
    </dl>
    <p className="text-xs text-muted-foreground">{spec.algorithmId} · версия {spec.algorithmVersion} · {spec.direction.toUpperCase()}. Период закрытия сигналов [{formatUnix(spec.fromUnix)}, {formatUnix(spec.toUnix)}). Инструментов: {spec.instruments.length}; TP×SL: {spec.tpLevels.length}×{spec.slLevels.length}. Seed: {spec.seed}; лимит времени: {spec.maxSeconds} с.</p>
    <section aria-label="Фиксированные требования исследования">
      <dl className="grid gap-2 text-sm sm:grid-cols-2">
        <div><dt className="text-muted-foreground">Минимум закрытых на ячейку</dt><dd>{spec.minClosedPerCell}</dd></div>
        <div><dt className="text-muted-foreground">Минимум сетапов в день</dt><dd>{frequencyThreshold}</dd></div>
      </dl>
    </section>
    <div className="flex flex-wrap gap-2">
      <Button type="button" variant="outline" onClick={controller.refresh}>Обновить статус</Button>
      {isActiveStudy(study) ? <Button type="button" variant="outline" disabled={study.status === "cancel_requested" || controller.cancelling !== null} onClick={() => { void controller.stop(study); }}>{controller.cancelling === study.id ? "Запрашиваем остановку…" : "Остановить исследование"}</Button>
        : <Button type="button" disabled={!controller.ready || controller.active !== null} onClick={onNew}>Новое исследование</Button>}
    </div>
    {study.status === "cancel_requested" && <Alert role="status">Сервер завершает текущие задачи и очистку. Остановка ещё не завершена.</Alert>}
    {study.error && <Alert variant="destructive" role="alert">{study.error.code}: {study.error.message}</Alert>}
    {study.status === "failed" && study.result === null && <p className="text-sm">Итог не сохранён. Промежуточный bestScore не восстанавливает потерянного кандидата; автоматического перезапуска нет.</p>}
    {result && <p className="text-sm">Причина завершения: {reasonLabels[result.reason]}.</p>}
    {result && !best && <Alert variant="warning">Нет оценимого кандидата, удовлетворившего требованиям всех выбранных ячеек и порогу «Минимум закрытых на ячейку»{spec.minSetupsPerDay !== undefined && spec.minSetupsPerDay > 0 ? " и частотному порогу «Минимум сетапов в день»" : ""}. Отсутствующий score не равен нулю.</Alert>}
    {best && <>
      <h3 className="font-semibold">Лучший score: {best.score} R</h3>
      <p className="text-sm">{best.partial ? "Частичный итог остановленного исследования." : "Лучший полностью проверенный кандидат в пределах бюджета."} Это не доказанный глобальный оптимум. Score — равное среднее R всех выбранных TP×SL-ячеек, а не выбор лучшей TP/SL-пары.</p>
      <section aria-label="Полные параметры результата" className="grid gap-2">
        <h4 className="font-medium">Полные параметры алгоритма</h4>
        <dl className="grid gap-2 text-sm sm:grid-cols-2">{Object.entries(best.params).map(([key, value]) => {
          const field = algorithm.fields.find((item) => item.key === key);
          const label = field ? `${field.label} (${field.unit})` : parameterLabels[key] ?? key;
          return <div key={key} className="break-words"><dt className="text-muted-foreground">{label}</dt><dd>{String(value)}</dd></div>;
        })}</dl>
        <h4 className="font-medium">Эффективная аналитика</h4>
        <dl className="grid gap-2 text-sm sm:grid-cols-2">{Object.entries({ ...best.analytics, setupMaturity: best.analytics.clusterEnabled ? spec.setupMaturity : null, horizonMinutes: spec.horizonMinutes }).map(([key, value]) => <div key={key}><dt className="text-muted-foreground">{analyticsLabels[key] ?? key}</dt><dd>{String(key === "clusterGapMinutes" && typeof value === "number" ? value / 60 : value)}</dd></div>)}</dl>
      </section>
      <section aria-label="Поддержка результата">
        <dl className="grid gap-2 text-sm sm:grid-cols-2">
          <div><dt className="text-muted-foreground">Выбрано сетапов</dt><dd>{best.support?.selectedSetupCount ?? "Не сохранено"}</dd></div>
          {typeof best.support?.avgSetupsPerDay === "number" && <div><dt className="text-muted-foreground">Среднее сетапов в день</dt><dd>{best.support.avgSetupsPerDay}</dd></div>}
        </dl>
      </section>
      <div className="overflow-x-auto">
        <Table aria-label="Матрица и поддержка результата">
          <TableHeader><TableRow>{["TP %", "SL %", "R", "Среднее %", "Закрытых", "TP", "SL", "Не закрыты", "Нет данных", "Гейт исключил"].map((title) => <TableHead key={title} numeric>{title}</TableHead>)}</TableRow></TableHeader>
          <TableBody>{best.matrix.map((cell) => <TableRow key={`${cell.tpPct}:${cell.slPct}`}>
            {[cell.tpPct, cell.slPct, cell.riskExpectancy, cell.avgMovePct, cell.closedCount, cell.tpCount, cell.slCount, cell.unclosedCount, cell.noDataCount, cell.priceChangeFilteredCount].map((value, index) => <TableCell key={index} numeric>{value ?? "—"}</TableCell>)}
          </TableRow>)}</TableBody>
        </Table>
      </div>
      <dl className="grid gap-2 text-xs sm:grid-cols-2">{Object.entries(best.provenance).map(([key, value]) => <div className="break-all" key={key}><dt className="text-muted-foreground">{key}</dt><dd>{String(value)}</dd></div>)}</dl>
      <p className="text-xs text-muted-foreground">Хеши фиксируют происхождение входов, но не гарантируют точного повторения после изменения истории. Комиссии, funding и проскальзывание не учитываются.</p>
      <ul className="grid gap-1 text-sm">{best.conclusions.slice(0, 32).map((item, index) => <li className="break-words" key={index}>{item.slice(0, 1000)}</li>)}</ul>
    </>}
    <details><summary className="cursor-pointer font-medium">Все входы исследования</summary><pre aria-label="Сохранённый spec" className="mt-2 max-h-96 overflow-auto whitespace-pre-wrap break-all rounded-md bg-muted p-3 text-xs">{JSON.stringify(spec, null, 2)}</pre></details>
    <section aria-label="Главные события исследования" className="grid gap-2">
      <h4 className="font-medium">Главные события · последние {Math.min(study.events.length, 50)}</h4>
      <ol className="grid max-h-64 gap-2 overflow-y-auto text-xs">{study.events.slice(-50).map((event, index) => <li key={`${event.at}:${index}`} className="break-words">{formatIsoDateTime(event.at)} · {event.kind} · {event.message.slice(0, 1000)}</li>)}</ol>
    </section>
    <p className="text-xs text-muted-foreground">Результат не применяется к live-алгоритму или настройкам аналитики. Закрытие окна не останавливает серверную работу.</p>
  </section>;
}
