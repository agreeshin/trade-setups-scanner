import { Alert } from "../components/ui/alert";
import { Button } from "../components/ui/button";
import { Card } from "../components/ui/card";
import { Pagination } from "../components/ui/pagination";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "../components/ui/table";
import { formatIsoDateTime } from "../lib/datetime-format";
import { localTime, localTimezoneLabel } from "./platform-controls";
import { usePlatformPoll } from "./use-platform-poll";

export const qualityReasonLabels = {
  missing_candle: "Нет свечи оцениваемой минуты",
  insufficient_warmup: "Недостаточно истории для прогрева",
  recent_gap: "Пропуск в недавней обязательной истории",
  price_reference_missing: "Нет опорной цены для расчёта роста",
  gap_tolerance_exceeded: "Превышен допуск пропусков",
};
export interface ReplayQuality {
  version: 1;
  state: "complete" | "partial" | "no_eligible_data" | "in_progress";
  requiredHistoryMinutes: number;
  requestedMinutes: number;
  evaluatedMinutes: number;
  evaluatedWithGapsMinutes: number;
  skippedMinutes: number;
  unassessedMinutes: number;
  assessedThroughUnix: number;
  reasonCounts: Record<keyof typeof qualityReasonLabels, number>;
}
export const qualityLabel = (quality: ReplayQuality | null | undefined): string => quality ? {
  complete: "Полностью оценён по настроенным допускам",
  partial: "Частично пригодные данные",
  no_eligible_data: "Нет пригодных данных",
  in_progress: "Оценена только обработанная часть",
}[quality.state] : "Качество не сохранено";

interface QualityIntervalPage {
  intervals: { fromUnix: number; toUnix: number; reason: "evaluated" | keyof typeof qualityReasonLabels }[];
  offset: number;
  limit: number;
  total: number;
  available: boolean;
}

function QualityIntervals({ id, offset, revision, onPage }: { id: number; offset: number; revision: number; onPage: (value: number) => void }) {
  const state = usePlatformPoll<QualityIntervalPage>(`/v1/replay-runs/${id}/quality-intervals?offset=${offset}&limit=50`, revision);
  if (state.kind === "loading") return <p role="status">Загрузка интервалов качества…</p>;
  if (state.kind === "unreachable") return <Alert variant="destructive" role="alert">Интервалы качества недоступны ({state.detail}). Это не означает отсутствие пропусков.</Alert>;
  if (!state.data.available) return <p>Интервалы качества не сохранены или исходный расчёт удалён. Если сводка качества сохранена, она продолжает описывать исходный расчёт.</p>;
  return <div className="mt-3 grid gap-3">
    <p className="text-xs text-muted-foreground">Интервалы [начало, конец): правая граница не включена. Время: {localTimezoneLabel()}. Пригодные интервалы могут содержать допускаемые настройками пропуски входного окна.</p>
    <div className="overflow-x-auto"><Table>
      <caption className="text-left">Интервалы оценки и пропусков</caption>
      <TableHeader><TableRow><TableHead>Начало</TableHead><TableHead>Конец (не включая)</TableHead><TableHead>Результат оценки</TableHead></TableRow></TableHeader>
      <TableBody>{state.data.intervals.slice(0, 50).map((interval, index) => <TableRow key={`${interval.fromUnix}-${index}`}>
        <TableCell>{localTime(interval.fromUnix)}</TableCell>
        <TableCell>{localTime(interval.toUnix)}</TableCell>
        <TableCell>{interval.reason === "evaluated" ? "Пригодно для оценки" : qualityReasonLabels[interval.reason]}</TableCell>
      </TableRow>)}</TableBody>
    </Table></div>
    {state.data.total === 0 && <p>Сохранённых интервалов пока нет.</p>}
    <Pagination
      page={Math.floor(offset / 50) + 1}
      pages={Math.max(1, Math.ceil(state.data.total / 50))}
      onPrev={() => onPage(Math.max(0, offset - 50))}
      onNext={() => onPage(offset + 50)}
      prevDisabled={offset === 0}
      nextDisabled={offset + 50 >= state.data.total}
      summary={`Интервалов: ${state.data.total}. Страница ${Math.floor(offset / 50) + 1} из ${Math.max(1, Math.ceil(state.data.total / 50))}`}
      label="Страницы интервалов качества"
      prevLabel="Предыдущие интервалы"
      nextLabel="Следующие интервалы"
    />
  </div>;
}

export interface QualityPanelState { open: boolean; offset: number }
export function ReplayQualitySummary({ run, panel, onPanel, revision }: {
  run: { id: number; cacheHit: boolean; quality?: ReplayQuality | null; qualityComputedAt?: string | null; qualitySourceRunId?: number | null };
  panel: QualityPanelState; onPanel: (value: QualityPanelState) => void; revision: number;
}) {
  const quality = run.quality;
  return <Card asChild><section className="mt-4 grid gap-3" aria-label="Качество расчёта">
    <h3 className="font-semibold">Качество расчёта: {qualityLabel(quality)}</h3>
    <p className="text-sm">{run.cacheHit ? "Из кеша" : "Результат этого запуска"}. Исходное вычисление завершено: {run.qualityComputedAt ? formatIsoDateTime(run.qualityComputedAt) : "время не сохранено или расчёт не завершён"}.{run.qualitySourceRunId != null && ` Исходный запуск: #${run.qualitySourceRunId}.`}</p>
    {run.cacheHit && <p className="text-xs text-muted-foreground">Качество относится к исходным данным кешированного расчёта. Последующая дозагрузка истории не обновляет эту сводку.</p>}
    {quality ? <>
      <p className="text-xs text-muted-foreground">Качество данных и статус выполнения различаются. Полная оценка означает пригодность всех минут по настроенным допускам, а не отсутствие любых пропусков входных данных.</p>
      <dl className="grid gap-2 text-sm sm:grid-cols-2">
        {[["Запрошено минут", quality.requestedMinutes], ["Рассмотрено минут", quality.evaluatedMinutes + quality.skippedMinutes], ["Оценено пригодных минут", quality.evaluatedMinutes], ["Из них с допустимыми пропусками окна", quality.evaluatedWithGapsMinutes], ["Пропущено непригодных минут", quality.skippedMinutes], ["Ещё не рассмотрено минут", quality.unassessedMinutes], ["Требуемая история (минут)", quality.requiredHistoryMinutes]].map(([label, value]) => <div key={label}><dt className="text-muted-foreground">{label}</dt><dd>{value}</dd></div>)}
        <div><dt className="text-muted-foreground">Граница рассмотренной части ({localTimezoneLabel()})</dt><dd>{localTime(quality.assessedThroughUnix)}</dd></div>
      </dl>
      {quality.unassessedMinutes > 0 && <p className="text-sm">Нерассмотренный остаток не является пропуском данных. Для отменённого или ошибочного запуска показан только сохранённый результат обработанной части.</p>}
      <details><summary className="cursor-pointer font-medium">Причины пропуска минут ({quality.skippedMinutes})</summary><dl className="mt-2 grid gap-2 text-sm sm:grid-cols-2">{(Object.keys(qualityReasonLabels) as (keyof typeof qualityReasonLabels)[]).map((reason) => <div key={reason}><dt className="text-muted-foreground">{qualityReasonLabels[reason]}</dt><dd>{quality.reasonCounts[reason]}</dd></div>)}</dl></details>
    </> : <p>Качество не сохранено для этого запуска. Нулевое число сетапов не доказывает полноту анализа или пригодность данных.</p>}
    <div><Button variant="outline" size="sm" aria-expanded={panel.open} onClick={() => onPanel({ ...panel, open: !panel.open })}>{panel.open ? "Скрыть интервалы качества" : "Показать интервалы качества"}</Button></div>
    {panel.open && <QualityIntervals key={`${run.id}-${panel.offset}`} id={run.id} offset={panel.offset} revision={revision} onPage={(offset) => onPanel({ open: true, offset })} />}
  </section></Card>;
}

export function EmptyReplayResult({ quality }: { quality: ReplayQuality | null | undefined }) {
  if (!quality) return <p className="mt-3 text-muted-foreground">Сетапов нет; качество и пригодность данных этого запуска неизвестны.</p>;
  if (quality.state === "no_eligible_data") return <p className="mt-3 text-muted-foreground">Нет пригодных данных для оценки. Это не успешный поиск с нулём сигналов.</p>;
  if (quality.evaluatedMinutes === 0) return <p className="mt-3 text-muted-foreground">В сохранённой обработанной части нет оценённых пригодных минут; отсутствие сигналов пока не является результатом полного анализа.</p>;
  return <p className="mt-3 text-muted-foreground">На {quality.evaluatedMinutes} пригодных оценённых минутах сигналы не найдены.{quality.state !== "complete" && " Результат не охватывает весь запрошенный диапазон."}</p>;
}
