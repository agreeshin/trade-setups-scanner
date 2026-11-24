import { useEffect, useId, useMemo, useState, useSyncExternalStore } from "react";
import { fetchForecastCurve, type ForecastCurveRequest, type ForecastCurveResponse } from "../lib/forecast-api";
import { forecastRecipeLabel } from "../lib/forecast-recipe";
import { buildDailyExpectancyModel, observedDailyPoints, type ObservedDailyMode, type DailyExpectancyPoint } from "../lib/daily-expectancy";
import { dayKeyOf, effectiveTimezone, formatUnix, interfaceSettings, nextZonedDay, subscribeInterfaceSettings, zonedDayStart } from "../lib/datetime-format";
import { Field } from "./ui/field";
import { Select } from "./ui/select";

const risk = (value: number | null) => value === null ? "—" : `${Number(value.toFixed(3))} R`;
export const analyticsDayTick = (unix: number) => { const day = dayKeyOf(unix); return `${day.slice(8, 10)}.${day.slice(5, 7)}`; };

export function DailyObservedControls({ value, onChange }: { value: ObservedDailyMode; onChange: (value: ObservedDailyMode) => void }) {
  return <Field label="Фактический ряд по дням" className="w-80 max-w-full" hint="Только точки и их шкала. Обучение, прогноз и матрица не меняются.">
    {(control) => <Select {...control} value={value} onChange={(event) => onChange(event.target.value === "sum" ? "sum" : "mean")}>
      <option value="mean">Среднее R закрытых сделок дня</option>
      <option value="sum">Сумма R закрытых сделок дня</option>
    </Select>}
  </Field>;
}

/** Null model prefixes/gaps are unavailable, not zeros and not interpolated bridges. */
function finiteSegments<T>(points: readonly T[], value: (point: T) => number | null): T[][] {
  const segments: T[][] = [];
  let current: T[] = [];
  for (const point of points) {
    const metric = value(point);
    if (metric !== null && Number.isFinite(metric)) current.push(point);
    else if (current.length > 0) { segments.push(current); current = []; }
  }
  if (current.length > 0) segments.push(current);
  return segments;
}

/** Request-owned geometry only: absent dates get grid lines, never observations.
 * Long cutoff/future spans use a repeating SVG tile instead of unbounded DOM. */
export function DailyCalendarGrid({ fromUnix, toUnix, x, top, bottom, local = false }: {
  fromUnix: number; toUnix: number; x: (atUnix: number) => number; top: number; bottom: number; local?: boolean;
}) {
  const patternId = useId();
  if (local) {
    const days: number[] = [];
    // Forecast signal spans are bounded to 3660 UTC days. Local boundaries can
    // be 23/25 hours apart; never feed these presentation instants to the API.
    for (let at = zonedDayStart(fromUnix); at <= toUnix && days.length < 3663; at = nextZonedDay(at)) days.push(at);
    return <g aria-hidden="true" data-testid="daily-calendar-grid">{days.map((at) => <line key={at} data-day-start-unix={at} className="stroke-muted-foreground/20" x1={x(at)} x2={x(at)} y1={top} y2={bottom} />)}</g>;
  }
  const first = Math.ceil(fromUnix / 86400) * 86400;
  const count = Math.max(0, Math.floor((toUnix - first) / 86400) + 1);
  if (count > 3661) {
    const firstX = x(first);
    const step = x(first + 86400) - firstX;
    return <g aria-hidden="true" data-testid="daily-calendar-grid">
      <defs><pattern id={patternId} patternUnits="userSpaceOnUse" x={firstX} y={top} width={step} height={bottom - top} viewBox={`0 0 ${step} ${bottom - top}`}>
        <path d={`M 0 0 V ${bottom - top}`} className="fill-none stroke-muted-foreground/20" />
      </pattern></defs>
      <rect x={x(fromUnix)} y={top} width={x(toUnix) - x(fromUnix)} height={bottom - top} fill={`url(#${patternId})`} />
    </g>;
  }
  return <g aria-hidden="true" data-testid="daily-calendar-grid">
    {Array.from({ length: count }, (_, index) => {
      const day = first + index * 86400;
      const at = x(day);
      return <line key={day} data-day-start-unix={day} className="stroke-muted-foreground/20" x1={at} x2={at} y1={top} y2={bottom} />;
    })}
  </g>;
}

/** Geometry only: F/B/M and canonical evaluation instants remain server values. */
export function AnalyticsForecastChart({ request: requested, days: requestedDays, observedMode: controlledMode, onObservedModeChange, retainWhileLoading = false }: {
  request: ForecastCurveRequest; days: DailyExpectancyPoint[]; observedMode?: ObservedDailyMode; onObservedModeChange?: (mode: ObservedDailyMode) => void; retainWhileLoading?: boolean;
}) {
  useSyncExternalStore(subscribeInterfaceSettings, interfaceSettings, interfaceSettings);
  const timezone = effectiveTimezone();
  const [localObservedMode, setLocalObservedMode] = useState<ObservedDailyMode>("mean");
  const observedMode = controlledMode ?? localObservedMode;
  const setObservedMode = (mode: ObservedDailyMode) => { setLocalObservedMode(mode); onObservedModeChange?.(mode); };
  const [result, setResult] = useState<{ request: ForecastCurveRequest; days: DailyExpectancyPoint[]; response: ForecastCurveResponse | null; error: string | null } | null>(null);
  useEffect(() => {
    const controller = new AbortController();
    const timer = window.setTimeout(() => {
      void fetchForecastCurve(requested, controller.signal)
        .then((response) => { if (!controller.signal.aborted) setResult({ request: requested, days: requestedDays, response, error: null }); })
        .catch((error: unknown) => { if (!controller.signal.aborted) setResult((previous) => ({
          request: retainWhileLoading && previous?.response ? previous.request : requested,
          days: retainWhileLoading && previous?.response ? previous.days : requestedDays,
          response: retainWhileLoading ? previous?.response ?? null : null,
          error: error instanceof Error ? error.message : "ошибка расчёта",
        })); });
    }, 250);
    return () => { window.clearTimeout(timer); controller.abort(); };
  }, [requested, requestedDays, retainWhileLoading]);
  const current = retainWhileLoading || result?.request === requested ? result : null;
  const response = current?.response ?? null;
  const request = retainWhileLoading && response && current ? current.request : requested;
  const days = retainWhileLoading && response && current ? current.days : requestedDays;
  const observed = useMemo(() => observedDailyPoints(days, observedMode), [days, observedMode]);
  const model = buildDailyExpectancyModel(observed, request.fromUnix);
  const values = observed.flatMap((day) => day.riskExpectancy === null ? [] : [day.riskExpectancy]);
  if (response) {
    for (const point of response.curve) if (point.forecastR !== null) values.push(point.forecastR);
    for (const point of response.blendCurve ?? []) if (point.decisionR !== null) values.push(point.decisionR);
    for (const value of [response.meanR, response.flatFuture.forecastR, response.flatFuture.decisionR]) if (value !== null) values.push(value);
  }
  const estimate = response?.estimate.status === "available" ? response.estimate.value : null;
  const width = 760, height = 300, left = 66, right = 28, top = 30, bottom = 56;
  const min = Math.min(0, ...values), max = Math.max(0, ...values);
  const low = min === max ? min - 0.5 : min, high = min === max ? max + 0.5 : max;
  const from = zonedDayStart(days[0]?.dayStartUnix ?? request.fromUnix);
  const to = zonedDayStart(days.at(-1)?.dayStartUnix ?? request.fromUnix);
  const x = (atUnix: number) => left + (to === from ? 0.5 : (atUnix - from) / (to - from)) * (width - left - right);
  const cohortX = (day: number) => x(zonedDayStart(day));
  const y = (value: number) => top + (high - value) / (high - low) * (height - top - bottom);
  const labelStep = Math.max(1, Math.ceil(days.length / 7));
  const display = response?.displayCurve ?? [];
  const forecastSegments = finiteSegments(display, (point) => point.forecastR);
  const decisionSegments = finiteSegments(display, (point) => point.decisionR);
  const endpoint = display.at(-1);
  return <section className="grid gap-2" aria-label="Вероятностный прогноз матожидания">
    <h3 className="text-sm font-medium">Дневное матожидание · Среднее за период M: <span aria-label="Среднее модели за период">{risk(response?.meanR ?? null)}</span> · Чистый прогноз F: <span aria-label="Чистый прогноз модели">{risk(response?.forecastR ?? null)}</span> · decisionR: <span aria-label="Метрика решения">{risk(response?.decisionR ?? null)}</span></h3>
    <p className="text-xs text-muted-foreground">{forecastRecipeLabel(request.forecast, "linear")} · q {request.q}. Cutoff C: {formatUnix(request.observationCutoff)} ({timezone}). Ретроспективная модельная оценка, не causal/OOS-прогноз.</p>
    <p className="text-xs text-muted-foreground">Происхождение: переданные закрытые TP/SL; доступность исходов утверждена на cutoff C. UTC-когорты исходных сигналов показаны в календаре {timezone} без пересчёта агрегатов, не как точные локальные исходы. Конечное значение на C показано на дне последнего включённого сигнала; продолжение до now скрыто. Новая ручная выборка может отличаться от frozen-рекомендации Telegram.</p>
    {response && response.displayCurve === undefined && <p role="status" className="text-xs text-muted-foreground">Модельный график недоступен: сервер не передал проекцию по дням сигналов. Канонические значения M/F/B сохранены.</p>}
    <p role="status" className={retainWhileLoading ? "min-h-8 text-xs text-muted-foreground" : "text-xs text-muted-foreground"}>{current?.request !== requested && !current?.error ? `Расчёт общей серверной модели…${response ? " Показана предыдущая завершённая итерация." : ""}` : ""}</p>
    {current?.error && <p role="status" className="text-xs text-danger">Модель недоступна: {current.error}. Подстановка тренда или среднего не выполняется.</p>}
    {response?.estimate.status === "unavailable" && <p role="status" className="text-xs text-muted-foreground">Прогноз недоступен: {response.estimate.reason === "no_closed_outcomes" ? "нет наблюдавшихся закрытых TP/SL" : "нет положительного веса наблюдений"}. Prior не заменяет реальные закрытия.</p>}
    {estimate && <p className="text-xs text-muted-foreground">Реальные закрытия: TP {estimate.tpCount}, SL {estimate.slCount}. Взвешенные: TP {estimate.weightedTp.toFixed(3)}, SL {estimate.weightedSl.toFixed(3)}, W {estimate.weightSum.toFixed(3)}. Вероятность TP {(estimate.tpProbability * 100).toFixed(2)}%; эвристический prior {(estimate.priorProbability * 100).toFixed(2)}%. κ не считается реальными сделками.</p>}
    <div className="flex flex-wrap gap-3 text-xs" aria-label="Легенда графика">
      <span>Факт: {observedMode === "mean" ? "дневные средние R" : "дневные суммы R"}</span><span className="text-warning">F: ретроспективная модель</span><span className="text-muted-foreground">M: среднее за период</span>
      {request.trendMode === "blend" && <span className="text-primary">B: смесь среднего и прогноза</span>}
      <span>Конец: последний день сигнала, значение модели на C</span>
    </div>
    <DailyObservedControls value={observedMode} onChange={setObservedMode} />
    <p className="text-xs text-muted-foreground sm:hidden">На узком экране график прокручивается по горизонтали.</p>
    <div data-testid="forecast-plot-viewport" role="region" aria-label="Область графика с горизонтальной прокруткой" tabIndex={0} className="min-w-0 overflow-x-auto">
    <svg viewBox={`0 0 ${width} ${height}`} role="img" aria-label="Дневное матожидание и вероятностная модель" className="w-full min-w-[640px]">
      <desc>Фактические серверные {observedMode === "mean" ? "дневные средние" : "дневные суммы R"} с календарными подписями {timezone} без пересчёта агрегатов, серверная модель F, горизонталь периодного среднего M и выбранная M/F/B. Только отображение привязано к дням UTC-когорт, не точным локальным исходам; конечное значение вычислено на cutoff C, без продолжения до now. История ретроспективна.</desc>
      <line className="stroke-muted-foreground/40" x1={left} y1={top} x2={left} y2={height - bottom} />
      <line className="stroke-muted-foreground/40" x1={left} y1={height - bottom} x2={width - right} y2={height - bottom} />
      {[low, 0, high].filter((value, index, all) => all.indexOf(value) === index).map((value) => <g key={value}>
        <line className="stroke-muted-foreground/20" x1={left} y1={y(value)} x2={width - right} y2={y(value)} />
        <text className="fill-muted-foreground text-[10px]" x={left - 8} y={y(value) + 4} textAnchor="end">{risk(value)}</text>
      </g>)}
      <DailyCalendarGrid local fromUnix={from} toUnix={to} x={x} top={top} bottom={height - bottom} />
      {response?.meanR != null && <line data-testid="period-mean" className="stroke-muted-foreground" strokeDasharray="2 4" x1={left} x2={width - right} y1={y(response.meanR)} y2={y(response.meanR)}><title>Среднее за период M: {risk(response.meanR)}</title></line>}
      {model.segments.filter((segment) => segment.length > 1).map((segment, index) => <polyline key={index} className="fill-none stroke-primary" points={segment.map((point) => `${cohortX(point.dayStartUnix)},${y(point.riskExpectancy!)}`).join(" ")} />)}
      {forecastSegments.map((segment, index) => segment.length > 1
        ? <polyline key={index} data-testid="forecast-model-curve" className="fill-none stroke-warning stroke-2" points={segment.map((point) => `${cohortX(point.signalDayUnix)},${y(point.forecastR!)}`).join(" ")}><title>Чистый прогноз F: серверная ретроспективная модель</title></polyline>
        : <circle key={index} data-testid="forecast-model-point" className="fill-warning" cx={cohortX(segment[0]!.signalDayUnix)} cy={y(segment[0]!.forecastR!)} r={3}><title>Чистый прогноз F: {risk(segment[0]!.forecastR)}</title></circle>)}
      {request.trendMode !== "forecast" && decisionSegments.map((segment, index) => segment.length > 1
        ? <polyline key={index} data-testid="forecast-blend-curve" className={`fill-none stroke-2 ${request.trendMode === "blend" ? "stroke-primary" : "stroke-muted-foreground"}`} strokeDasharray="8 3" points={segment.map((point) => `${cohortX(point.signalDayUnix)},${y(point.decisionR!)}`).join(" ")}><title>Выбранный objective {request.trendMode === "mean" ? "M" : "B"}: {risk(response?.decisionR ?? null)}</title></polyline>
        : <circle key={index} data-testid="forecast-blend-point" className={request.trendMode === "blend" ? "fill-primary" : "fill-muted-foreground"} cx={cohortX(segment[0]!.signalDayUnix)} cy={y(segment[0]!.decisionR!)} r={3} />)}
      {endpoint?.decisionR != null && <text data-testid="forecast-endpoint-label" data-canonical-at-unix={endpoint.atUnix} data-signal-day-unix={endpoint.signalDayUnix}
        aria-label={`Конечное значение ${request.trendMode === "mean" ? "M" : request.trendMode === "blend" ? "B" : "F"}: ${risk(endpoint.decisionR)}; день сигнала ${formatUnix(endpoint.signalDayUnix, { time: false })}; cutoff ${formatUnix(endpoint.atUnix)}`}
        className={`font-mono text-[10px] ${request.trendMode === "forecast" ? "fill-warning" : request.trendMode === "blend" ? "fill-primary" : "fill-muted-foreground"}`} textAnchor={to === from ? "middle" : "end"} x={cohortX(endpoint.signalDayUnix)} y={y(endpoint.decisionR) - 10}>{risk(endpoint.decisionR)}</text>}
      {model.points.map((point, index) => <g key={point.day}>
        {point.riskExpectancy !== null && <circle className={point.riskExpectancy >= 0 ? "fill-success" : "fill-danger"} cx={cohortX(point.dayStartUnix)} cy={y(point.riskExpectancy)} r={4}><title>{formatUnix(point.dayStartUnix, { time: false })} ({timezone}): {observedMode === "sum" ? "сумма " : ""}{risk(point.riskExpectancy)}, TP {point.tpCount}, SL {point.slCount}</title></circle>}
        {(index % labelStep === 0 || index === model.points.length - 1) && <text aria-label={formatUnix(point.dayStartUnix, { time: false })} className="fill-muted-foreground text-[10px]" x={cohortX(point.dayStartUnix)} y={height - 27} textAnchor="middle">{analyticsDayTick(point.dayStartUnix)}<title>{formatUnix(point.dayStartUnix, { time: false })}</title></text>}
      </g>)}
      <text className="fill-muted-foreground text-[10px]" x={(left + width - right) / 2} y={height - 8} textAnchor="middle">Дата исходного сигнала ({timezone})</text>
      <text className="fill-muted-foreground text-[10px]" x={left} y={top - 10}>{observedMode === "mean" ? "Среднее R дня" : "Сумма R дня"}</text>
    </svg>
    </div>
  </section>;
}
