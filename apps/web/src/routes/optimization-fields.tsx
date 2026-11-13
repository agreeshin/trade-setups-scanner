import { Checkbox } from "../components/ui/checkbox";
import { Field } from "../components/ui/field";
import { Input } from "../components/ui/input";
import { Select } from "../components/ui/select";
import { InstrumentPicker, type CatalogInstrument } from "./instrument-picker";
import { ANALYTICS_SEARCH_FIELDS, type DomainDrafts, type ScopeDraft, type SearchField } from "./optimization-form";
import type { Algorithm } from "./platform-controls";

function DomainControls({ fields, value, onChange }: { fields: SearchField[]; value: DomainDrafts; onChange: (next: DomainDrafts) => void }) {
  return <div className="grid gap-3">{fields.map((field) => {
    const draft = value[field.key]!;
    const update = (patch: Partial<typeof draft>) => onChange({ ...value, [field.key]: { ...draft, ...patch } });
    return <fieldset key={field.key} className="grid min-w-0 gap-3 rounded-md border border-input p-3 sm:grid-cols-2">
      <legend className="px-1 font-medium">{field.label}{field.unit ? ` (${field.unit})` : ""}</legend>
      <Field label={`Baseline: ${field.label}`} hint={field.type === "boolean" ? "true / false" : field.type === "nullable" ? "null — гейт отключён; 0 — отдельный числовой порог" : `${field.min}..${field.max}${field.type === "integer" ? ", целое" : ", десятичная точка"}`}>
        {(control) => field.type === "boolean" ? <Select {...control} value={draft.baseline} onChange={(event) => update({ baseline: event.target.value })}><option value="true">Включена</option><option value="false">Выключена</option></Select>
          : <Input {...control} maxLength={128} value={draft.baseline} onChange={(event) => update({ baseline: event.target.value })} />}
      </Field>
      <div className="grid gap-2">
        <label className="flex items-center gap-2 text-sm"><Checkbox checked={draft.search} onCheckedChange={(checked) => update({ search: checked === true })} />Подбирать: {field.label}</label>
        <Field label={`Значения: ${field.label}`} hint="Через запятую, до 256 значений. Выключенный подбор фиксирует baseline.">
          {(control) => <Input {...control} disabled={!draft.search} maxLength={8192} value={draft.values} onChange={(event) => update({ values: event.target.value })} />}
        </Field>
      </div>
    </fieldset>;
  })}</div>;
}

/** Identical metadata/baseline/domain controls for manual and continuous research.
 * Only the period, frequency-list and locked causal maturity differ. */
export function OptimizationFields({ algorithm, scope, onScopeChange, catalog, ready, disabled, algorithmDomains, onAlgorithmDomainsChange, analyticsDomains, onAnalyticsDomainsChange, continuous = false }: {
  algorithm: Algorithm; scope: ScopeDraft; onScopeChange: (patch: Partial<ScopeDraft>) => void; catalog: CatalogInstrument[];
  ready: boolean; disabled: boolean; algorithmDomains: DomainDrafts; onAlgorithmDomainsChange: (next: DomainDrafts) => void;
  analyticsDomains: DomainDrafts; onAnalyticsDomainsChange: (next: DomainDrafts) => void; continuous?: boolean;
}) {
  const numericFields: { key: "setupMaturity" | "horizonMinutes" | "minClosedPerCell" | "minSetupsPerDay" | "seed" | "maxTrials" | "maxSeconds"; label: string; min: number; max?: number; step?: number; hint?: string }[] = [
    { key: "setupMaturity", label: "Зрелость (фиксирована)", min: 0, max: 1, step: 0.1 },
    { key: "horizonMinutes", label: "Горизонт сделки, мин (фиксирован)", min: 1, max: 43200 },
    { key: "minClosedPerCell", label: "Минимум закрытых на ячейку", min: 1 },
    { key: "minSetupsPerDay", label: "Минимум сетапов в день", min: 0, step: 0.1, hint: "Среднее по всем выбранным инструментам за весь точный выбранный период сигналов, включая дробные дни и интервалы без сигналов. 0 отключает только частотный гейт." },
    { key: "seed", label: "Seed", min: 0 }, { key: "maxTrials", label: "Бюджет trials", min: 1, max: 4096 },
    { key: "maxSeconds", label: "Бюджет времени, секунд", min: 1, max: 86400 },
  ];
  return <fieldset disabled={disabled} className="grid min-w-0 gap-4">
    <legend className="mb-2 font-semibold">Фиксированный scope</legend>
    {!continuous && <p className="text-xs text-muted-foreground">Часовой пояс ввода: {Intl.DateTimeFormat().resolvedOptions().timeZone} (часовой пояс браузера). Период по времени закрытия сигнала [начало, конец), границы кратны минуте. Горизонт сделки не обрезается концом периода.</p>}
    <div className="grid gap-3 sm:grid-cols-2">
      {!continuous && <>
        <Field label="Начало периода сигналов">{(control) => <Input {...control} type="datetime-local" step={60} value={scope.from} onChange={(event) => onScopeChange({ from: event.target.value })} />}</Field>
        <Field label="Конец периода сигналов">{(control) => <Input {...control} type="datetime-local" step={60} value={scope.to} onChange={(event) => onScopeChange({ to: event.target.value })} />}</Field>
      </>}
      <Field label="Направление сделки">{(control) => <Select {...control} value={scope.direction} onChange={(event) => onScopeChange({ direction: event.target.value === "long" ? "long" : "short" })}><option value="short">SHORT</option><option value="long">LONG</option></Select>}</Field>
    </div>
    <InstrumentPicker catalog={catalog} symbols={scope.symbols} onChange={(symbols) => onScopeChange({ symbols })} disabled={!ready || disabled} limit={null} />
    <div className="grid gap-3 sm:grid-cols-2">
      <Field label="Уровни TP, %" hint="1..64 уникальных значений через запятую; 0 < TP ≤ 100.">{(control) => <Input {...control} maxLength={8192} value={scope.tpLevels} onChange={(event) => onScopeChange({ tpLevels: event.target.value })} />}</Field>
      <Field label="Уровни SL, %" hint="Независимая ось: 0 < SL ≤ 100, не более 64 уровней.">{(control) => <Input {...control} maxLength={8192} value={scope.slLevels} onChange={(event) => onScopeChange({ slLevels: event.target.value })} />}</Field>
      {numericFields.filter((field) => !continuous || field.key !== "minSetupsPerDay").map((field) => <Field key={field.key} label={field.label} hint={continuous && field.key === "setupMaturity" ? "Только первый сетап кластера. При выключенной кластеризации не применяется." : field.hint}>{(control) => <Input {...control} type="number" min={field.min} max={field.max} step={field.step ?? 1} disabled={continuous && field.key === "setupMaturity"} value={continuous && field.key === "setupMaturity" ? "0" : scope[field.key]} onChange={(event) => onScopeChange({ [field.key]: event.target.value })} />}</Field>)}
    </div>
    <p className="text-xs text-muted-foreground">Выбрано ячеек TP×SL: {scope.tpLevels.split(",").filter((item) => item.trim()).length} × {scope.slLevels.split(",").filter((item) => item.trim()).length}. Зрелость не подбирается; без кластеризации не применяется. Гейт null отличается от порога 0.</p>
    <h3 className="font-semibold">Параметры алгоритма · {algorithm.version}</h3>
    <p className="text-xs text-muted-foreground">Baseline скопирован из сохранённой конфигурации, а не из несохранённых правок карточки. Изменения здесь относятся только к исследованию. Составные ограничения проверяет сервер.</p>
    <DomainControls fields={algorithm.fields} value={algorithmDomains} onChange={onAlgorithmDomainsChange} />
    {Object.entries(algorithm.params).filter(([key]) => !algorithm.fields.some((field) => field.key === key)).map(([key, value]) => <p key={key} className="text-sm">{key}: {value} (фиксирован)</p>)}
    <h3 className="font-semibold">Параметры аналитики</h3>
    <DomainControls fields={ANALYTICS_SEARCH_FIELDS} value={analyticsDomains} onChange={onAnalyticsDomainsChange} />
  </fieldset>;
}
