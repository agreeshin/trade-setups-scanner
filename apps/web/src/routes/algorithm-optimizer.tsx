import { useEffect, useRef, useState, type FormEvent } from "react";
import { Alert } from "../components/ui/alert";
import { Button } from "../components/ui/button";
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from "../components/ui/dialog";
import { Spinner } from "../components/ui/spinner";
import { fetchPlatform } from "../lib/platform-api";
import { getOptimizationDraft, isActiveStudy, type OptimizationStudyAction } from "../lib/optimization-api";
import type { CatalogInstrument } from "./instrument-picker";
import { algorithmDisplayName, errorText, localDateInput, type Algorithm } from "./platform-controls";
import { ANALYTICS_BASELINE, ANALYTICS_SEARCH_FIELDS, buildOptimizationSpec, initialDomains, initialScope, type ScopeDraft } from "./optimization-form";
import { OptimizationFields } from "./optimization-fields";
import { OptimizationResults } from "./optimization-results";
import { OptimizationTrashIcon } from "./optimization-history";
import { useOptimizationDetail } from "./use-optimization-detail";
import type { OptimizationController } from "./use-optimization-studies";
import { applyOptimizationDraftScope, createOptimizationDraft, restoreOptimizationDraft, useOptimizationDraftSave, type OptimizationFormDraft } from "./optimization-draft";

function OptimizerForm({ algorithm, controller, onStarted }: { algorithm: Algorithm; controller: OptimizationController; onStarted: () => void }) {
  const [algorithmDomains, setAlgorithmDomains] = useState(() => initialDomains(algorithm.fields, algorithm.params));
  const [analyticsDomains, setAnalyticsDomains] = useState(() => initialDomains(ANALYTICS_SEARCH_FIELDS, { ...ANALYTICS_BASELINE }));
  const [scope, setScope] = useState<ScopeDraft>(initialScope);
  const [catalog, setCatalog] = useState<CatalogInstrument[]>([]);
  const [ready, setReady] = useState(false);
  const [loadError, setLoadError] = useState<string | null>(null);
  const [revision, setRevision] = useState(0);
  const [error, setError] = useState<string | null>(null);
  const storedDraft = useRef<OptimizationFormDraft | null>(null);
  const draftSave = useOptimizationDraftSave(algorithm.id);
  useEffect(() => {
    const controller = new AbortController();
    void Promise.all([
      fetchPlatform<{ instruments: CatalogInstrument[] }>("/v1/instruments", controller.signal),
      fetchPlatform<{ value: { replay?: { symbols?: string; instruments?: { symbol: string; exchange: string; category: string }[]; fromUnix?: number; toUnix?: number; from?: string; to?: string } } }>("/v1/settings/ui", controller.signal),
      getOptimizationDraft(algorithm.id, controller.signal),
    ]).then(([instruments, preferences, rawDraft]) => {
      if (controller.signal.aborted) return;
      if (instruments.kind !== "ready" || preferences.kind !== "ready") {
        setLoadError("Не удалось загрузить historical-каталог или сохранённый выбор. Запуск заблокирован; повторите загрузку.");
        return;
      }
      const items = instruments.data.instruments.filter((item) => item.exchange === "bybit" && item.category === "linear");
      const saved = preferences.data.value.replay;
      const fallbackScope = { ...initialScope(),
        symbols: typeof saved?.symbols === "string" ? saved.symbols : (saved?.instruments ?? items).filter((item) => item.exchange === "bybit" && item.category === "linear").map((item) => item.symbol).join("\n"),
        from: typeof saved?.fromUnix === "number" ? localDateInput(saved.fromUnix) : saved?.from ?? "",
        to: typeof saved?.toUnix === "number" ? localDateInput(saved.toUnix) : saved?.to ?? "",
      };
      const fallbackAlgorithmDomains = initialDomains(algorithm.fields, algorithm.params);
      const fallbackAnalyticsDomains = initialDomains(ANALYTICS_SEARCH_FIELDS, { ...ANALYTICS_BASELINE });
      const restored = restoreOptimizationDraft(rawDraft, algorithm.fields, ANALYTICS_SEARCH_FIELDS, fallbackScope, fallbackAlgorithmDomains, fallbackAnalyticsDomains);
      storedDraft.current = restored;
      setCatalog(items);
      setScope(restored ? applyOptimizationDraftScope(fallbackScope, restored, "manual") : fallbackScope);
      setAlgorithmDomains(restored?.shared.algorithmDomains ?? fallbackAlgorithmDomains);
      setAnalyticsDomains(restored?.shared.analyticsDomains ?? fallbackAnalyticsDomains);
      setLoadError(null); setReady(true);
    }).catch(() => {
      if (!controller.signal.aborted) setLoadError("Не удалось загрузить сохранённые настройки оптимизации. Запуск заблокирован; повторите загрузку.");
    });
    return () => controller.abort();
  }, [algorithm, revision]);
  const change = (patch: Partial<ScopeDraft>) => { setScope({ ...scope, ...patch }); setError(null); };
  async function submit(event: FormEvent<HTMLFormElement>) {
    event.preventDefault();
    if (!ready || controller.creating || controller.active || !controller.ready) return;
    setError(null);
    try {
      const spec = buildOptimizationSpec(algorithm, scope, catalog, algorithmDomains, analyticsDomains);
      if (await controller.start(spec)) onStarted();
    } catch (cause) { setError(errorText(cause)); }
  }
  function saveDraft(): void {
    if (!ready || controller.creating) return;
    const next = createOptimizationDraft(scope, algorithmDomains, analyticsDomains, storedDraft.current, {
      manual: { from: scope.from, to: scope.to, setupMaturity: scope.setupMaturity, minSetupsPerDay: scope.minSetupsPerDay },
    });
    storedDraft.current = next;
    draftSave.save(next);
  }
  return <form noValidate onSubmit={(event) => { void submit(event); }} onBlur={saveDraft} className="grid min-w-0 gap-5">
    <Alert>Исследование изолировано от live, обычных прогонов и кешей. Все выбранные TP×SL-ячейки имеют равный вес; baseline оценивается как trial 0 и входит в бюджет. Открытие и изменение формы ничего не запускают.</Alert>
    {!ready && !loadError && <Spinner label="Загрузка historical-списка" />}
    {loadError && <Alert variant="destructive" role="alert">{loadError}<Button type="button" variant="outline" onClick={() => setRevision((value) => value + 1)}>Повторить загрузку</Button></Alert>}
    <OptimizationFields algorithm={algorithm} scope={scope} onScopeChange={change} catalog={catalog} ready={ready} disabled={controller.creating} algorithmDomains={algorithmDomains} onAlgorithmDomainsChange={setAlgorithmDomains} analyticsDomains={analyticsDomains} onAnalyticsDomainsChange={setAnalyticsDomains} />
    {(error ?? draftSave.saveError) && <Alert variant="destructive" role="alert">{error ?? draftSave.saveError}</Alert>}
    <div><Button type="submit" disabled={!ready || !controller.ready || controller.creating || controller.active !== null}>{controller.creating ? "Создание исследования…" : "Начать исследование"}</Button></div>
  </form>;
}

function StudyDetail({ detail, algorithm, controller, onDelete, onNew }: {
  detail: ReturnType<typeof useOptimizationDetail>; algorithm: Algorithm; controller: OptimizationController;
  onDelete: (study: OptimizationStudyAction) => void; onNew?: () => void;
}) {
  return <>
    {detail?.error && <Alert variant="destructive" role="alert">{detail.error}<Button type="button" variant="outline" onClick={controller.refresh}>Повторить загрузку результата</Button></Alert>}
    {!detail?.study && !detail?.error && <Spinner label="Загрузка исследования" />}
    {detail?.study && <>
      <OptimizationResults study={detail.study} algorithm={algorithm} controller={controller} {...(onNew ? { onNew } : {})} />
      {!isActiveStudy(detail.study) && <div><Button type="button" variant="outline" disabled={controller.creating || controller.cancelling !== null || controller.deleting !== null} aria-label={`Удалить исследование ${detail.study.id}`} onClick={() => onDelete(detail.study!)}><OptimizationTrashIcon />Удалить исследование</Button></div>}
    </>}
  </>;
}

// A history selection has its own sibling dialog/state. It never follows "latest"
// and closes/reopens the exact ID even when discovery finds a newer study.
export function OptimizationStudyDialog({ id, algorithm, controller, open, onOpenChange, returnFocusId, fallbackFocusId = returnFocusId, onDelete }: {
  id: number | undefined; algorithm: Algorithm; controller: OptimizationController; open: boolean;
  onOpenChange: (open: boolean) => void; returnFocusId: string; fallbackFocusId?: string; onDelete: (study: OptimizationStudyAction) => void;
}) {
  const detail = useOptimizationDetail(id, open, controller.revision);
  return <Dialog open={open} onOpenChange={onOpenChange}>
    <DialogContent className="max-w-4xl" onCloseAutoFocus={(event) => { event.preventDefault(); (document.getElementById(returnFocusId) ?? document.getElementById(fallbackFocusId))?.focus(); }}>
      <DialogHeader><DialogTitle>Исследование #{id} · {algorithmDisplayName(algorithm)}</DialogTitle><DialogDescription>Сохранённое исследование. Просмотр и закрытие окна не меняют серверную работу или настройки.</DialogDescription></DialogHeader>
      <div className="grid min-h-0 min-w-0 flex-1 gap-4 overflow-y-auto pr-1">
        {controller.error && <Alert variant="destructive" role="alert">{controller.error}</Alert>}
        <StudyDetail detail={detail} algorithm={algorithm} controller={controller} onDelete={onDelete} />
      </div>
    </DialogContent>
  </Dialog>;
}

export function AlgorithmOptimizer({ algorithm, controller, open, onOpenChange, returnFocusId, onDelete }: { algorithm: Algorithm; controller: OptimizationController; open: boolean; onOpenChange: (open: boolean) => void; returnFocusId: string; onDelete: (study: OptimizationStudyAction) => void }) {
  const study = controller.byAlgorithm[algorithm.id];
  const [drafting, setDrafting] = useState(false);
  const showStudy = study && (isActiveStudy(study) || !drafting);
  const detail = useOptimizationDetail(study?.id, open && Boolean(showStudy), controller.revision, study && !isActiveStudy(study) ? study.updatedAt : undefined);
  return <Dialog open={open} onOpenChange={(next) => { if (!next) setDrafting(false); onOpenChange(next); }}>
    <DialogContent className="max-w-4xl" onCloseAutoFocus={(event) => { event.preventDefault(); document.getElementById(returnFocusId)?.focus(); }}>
      <DialogHeader>
        <DialogTitle>Оптимизация · {algorithmDisplayName(algorithm)}</DialogTitle>
        <DialogDescription>Серверное исследование Plan A. Закрытие окна не останавливает работу; для остановки нужна отдельная команда.</DialogDescription>
      </DialogHeader>
      <div className="grid min-h-0 min-w-0 flex-1 gap-4 overflow-y-auto pr-1">
        {controller.error && <Alert variant="destructive" role="alert">{controller.error}</Alert>}
        {!controller.ready && <div className="flex flex-wrap items-center gap-2 text-sm"><span>Проверяем сохранённые исследования. Новый запуск пока недоступен.</span><Button type="button" variant="outline" onClick={controller.refresh}>Обновить статус</Button></div>}
        {controller.active && controller.active.spec.algorithmId !== algorithm.id && <Alert variant="warning">Исследование #{controller.active.id} для {controller.active.spec.algorithmId} уже выполняется. В платформе допускается только одно активное исследование.</Alert>}
        {showStudy ? <StudyDetail detail={detail} algorithm={algorithm} controller={controller} onDelete={onDelete} onNew={() => setDrafting(true)} />
          : <OptimizerForm algorithm={algorithm} controller={controller} onStarted={() => setDrafting(false)} />}
      </div>
    </DialogContent>
  </Dialog>;
}
