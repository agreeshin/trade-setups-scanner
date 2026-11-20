import { RefreshCw } from "lucide-react";
import { Badge } from "../components/ui/badge";
import { Button } from "../components/ui/button";
import { IconButton } from "../components/ui/icon-button";
import { Tooltip } from "../components/ui/tooltip";
import { formatUnix } from "../lib/datetime-format";
import type { StudyActualStage } from "../lib/study-actual-api";
import { formatOutcomeRisk, outcomeMeanClass } from "./setup-outcomes-table";
import type { useStudyActualAction } from "./use-study-actual-action";

type ActualAction = ReturnType<typeof useStudyActualAction>;
const stageLabels: Record<StudyActualStage, string> = { queued: "В очереди", acquiring: "Подготовка данных", replaying: "Исторический прогон", evaluating: "Расчёт исходов", publishing: "Сохранение результата", completed: "Готово", failed: "Ошибка расчёта" };

export function StudyActualFact({ action }: { action: ActualAction }) {
  const { actual, pending, submitting, uncertain, error } = action;
  return <div className="grid gap-1 text-sm">
    {pending ? <span role="status" className="text-muted-foreground">{uncertain ? "Статус расчёта уточняется…" : submitting ? "Отправка запроса…" : actual?.stage ? stageLabels[actual.stage] : "В очереди"}</span>
      : actual?.status === "completed" && actual.counts !== null ? <>
        <div className="flex flex-wrap items-center gap-2">
          <span className={`font-medium tabular-nums ${outcomeMeanClass(actual.meanR)}`}>{formatOutcomeRisk(actual.meanR)}</span>
          <Tooltip content={<div className="grid gap-1"><p>Закрытые сделки / все сетапы</p><p>TP: {actual.counts.tp} · SL: {actual.counts.sl} · гейт не пройден: {actual.counts.gateFailed} · не закрылись: {actual.counts.unclosed} · нет данных: {actual.counts.noData}</p>{actual.observedThroughUnix !== null && <p>Данные до: {formatUnix(actual.observedThroughUnix)}</p>}</div>}>
            <Badge tabIndex={0} className="bg-ambient-violet/15 text-ambient-violet" aria-label={`Закрытые сделки / все сетапы: ${actual.counts.closed}/${actual.counts.total}`}>{actual.counts.closed}/{actual.counts.total}</Badge>
          </Tooltip>
        </div>
        {actual.meanR === null && <span className="text-xs text-muted-foreground">{actual.counts.total === 0 ? "Нет сетапов в периоде" : "Нет закрытых сделок"}</span>}
      </> : actual?.status === "failed" ? <span role="alert" className="text-danger">{actual.error ? `${actual.error.code}: ${actual.error.message.slice(0, 300)}` : "Расчёт завершился ошибкой. Повторите запуск вручную."}</span>
        : <span className="text-muted-foreground">Не рассчитано</span>}
    {error && <span role="alert" className="text-danger">{error}</span>}
    {uncertain && <Button type="button" variant="outline" size="sm" disabled={submitting} onClick={() => { void action.reconcile(); }}>Проверить статус расчёта</Button>}
  </div>;
}

export function StudyActualRefreshButton({ studyId, action }: { studyId: number; action: ActualAction }) {
  const unavailable = !action.actual?.eligibility.eligible;
  const reason = action.actual?.eligibility.reason ?? "Сервер не подтвердил доступность расчёта.";
  return <IconButton label={unavailable ? `Расчёт после исследования ${studyId} недоступен: ${reason.slice(0, 200)}` : `Пересчитать матожидание после исследования ${studyId}`}
    icon={RefreshCw} spinning={action.pending} disabled={unavailable || action.pending} aria-busy={action.pending} onClick={() => { void action.refresh(); }} />;
}
