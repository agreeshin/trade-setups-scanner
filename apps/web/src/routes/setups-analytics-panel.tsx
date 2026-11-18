import { useEffect, useMemo, useRef, useState } from "react";
import { Alert, AlertDescription } from "../components/ui/alert";
import { Badge } from "../components/ui/badge";
import { Button } from "../components/ui/button";
import { Card, CardContent } from "../components/ui/card";
import { Checkbox } from "../components/ui/checkbox";
import { DateInput } from "../components/ui/date-input";
import { Dialog, DialogContent, DialogDescription, DialogHeader, DialogTitle } from "../components/ui/dialog";
import { EmptyState } from "../components/ui/empty-state";
import { Field } from "../components/ui/field";
import { InfoNote } from "../components/ui/info-note";
import { Select } from "../components/ui/select";
import { Spinner } from "../components/ui/spinner";
import { Table, TableBody, TableCell, TableHead, TableHeader, TableRow } from "../components/ui/table";
import { fetchPlatform } from "../lib/platform-api";
import {
  fetchSetupsAnalytics,
  fetchSetupsAnalyticsProfiles,
  type SetupsAnalyticsProfile,
  type SetupsAnalyticsResult,
  type SetupsAnalyticsSetup,
  type SetupsAnalyticsStatus,
} from "../lib/setups-analytics-api";
import {
  algorithmDisplayName,
  errorText,
  localDateInput,
  localTime,
  localTimezoneLabel,
  replayRange,
  utcMidnightOnDateChange,
  type Algorithm,
} from "./platform-controls";

const PROFILES_DEBOUNCE_MS = 400;

function formatPrice(value: number | null): string {
  if (value === null) return "—";
  return String(Number(value.toPrecision(6)));
}

function formatRisk(value: number | null): string {
  if (value === null) return "—";
  return `${value >= 0 ? "+" : ""}${value.toFixed(2)}R`;
}

function formatTime(unix: number | null): string {
  if (unix === null) return "—";
  return localTime(unix);
}

function meanColorClass(value: number | null): string {
  if (value === null || value === 0) return "text-muted-foreground";
  return value > 0 ? "text-success" : "text-danger";
}

const STATUS_BADGES: Record<SetupsAnalyticsStatus, { label: string; variant: "success" | "danger" | "neutral" | "warning" }> = {
  tp: { label: "TP", variant: "success" },
  sl: { label: "SL", variant: "danger" },
  unclosed: { label: "не закрылась", variant: "neutral" },
  no_data: { label: "нет данных", variant: "neutral" },
  gate_failed: { label: "гейт не пройден", variant: "warning" },
};

function GateBadge({ passed, selected }: { passed: number; selected: number }) {
  return <Badge className="bg-ambient-violet/15 text-ambient-violet" aria-label={`Прошли гейт: ${passed} из ${selected}`}>{passed}/{selected}</Badge>;
}

function SetupOutcomeRow({ setup, onOpenProfile }: { setup: SetupsAnalyticsSetup; onOpenProfile: (slotId: number) => void }) {
  const status = STATUS_BADGES[setup.status];
  return <TableRow>
    <TableCell className="font-medium">{setup.symbol}</TableCell>
    <TableCell className="whitespace-nowrap tabular-nums">{formatTime(setup.setupTimeUnix)}</TableCell>
    <TableCell numeric>{formatPrice(setup.entryPrice)}</TableCell>
    <TableCell numeric className="text-success">{formatPrice(setup.tpPrice)}</TableCell>
    <TableCell numeric className="text-danger">{formatPrice(setup.slPrice)}</TableCell>
    <TableCell className="whitespace-nowrap tabular-nums">{formatTime(setup.closeTimeUnix)}</TableCell>
    <TableCell numeric className={(setup.riskReturn ?? 0) >= 0 ? "text-success" : "text-danger"}>{formatRisk(setup.riskReturn)}</TableCell>
    <TableCell><Badge variant={status.variant}>{status.label}</Badge></TableCell>
    <TableCell numeric>
      <button type="button" className="font-medium text-primary underline-offset-4 hover:underline" onClick={() => onOpenProfile(setup.slotId)}>№{setup.slotId}</button>
    </TableCell>
  </TableRow>;
}

function ProfileDetailsDialog({ profile, onClose }: { profile: SetupsAnalyticsProfile | null; onClose: () => void }) {
  return <Dialog open={profile !== null} onOpenChange={(open) => { if (!open) onClose(); }}>
    {profile && <DialogContent aria-label={`Профиль №${profile.slotId}`}>
      <DialogHeader>
        <DialogTitle>Профиль №{profile.slotId}</DialogTitle>
        <DialogDescription>{profile.externalName}</DialogDescription>
      </DialogHeader>
      <dl className="grid gap-2 text-sm sm:grid-cols-2">
        <div><dt className="text-muted-foreground">Исследование-основа</dt><dd>№{profile.studyId ?? "—"}</dd></div>
        <div><dt className="text-muted-foreground">Поколение выпуска</dt><dd>{profile.generationId ?? "—"}</dd></div>
        <div><dt className="text-muted-foreground">Среднее матрицы mean R</dt><dd className={`tabular-nums ${meanColorClass(profile.meanR)}`}>{formatRisk(profile.meanR)}</dd></div>
        <div><dt className="text-muted-foreground">Направление</dt><dd><Badge variant={profile.direction === "short" ? "danger" : "success"}>{profile.direction.toUpperCase()}</Badge></dd></div>
        <div><dt className="text-muted-foreground">TP / SL</dt><dd className="tabular-nums">{profile.tpPct}% / {profile.slPct}%</dd></div>
        <div><dt className="text-muted-foreground">Time-shift</dt><dd className="tabular-nums">{profile.timeShiftMinutes} мин</dd></div>
        <div><dt className="text-muted-foreground">Порог смещения цены</dt><dd className="tabular-nums">{profile.priceChangePct === null ? "не задан" : `${profile.priceChangePct}%`}</dd></div>
        <div><dt className="text-muted-foreground">Сетапов в диапазоне</dt><dd className="tabular-nums">{profile.setupCount}</dd></div>
      </dl>
    </DialogContent>}
  </Dialog>;
}

function ProfilePickerDialog({ open, profiles, selection, onSelectionChange, onClose, onApply, onOpenDetails }: {
  open: boolean;
  profiles: SetupsAnalyticsProfile[];
  selection: ReadonlySet<number>;
  onSelectionChange: (slotId: number, checked: boolean) => void;
  onClose: () => void;
  onApply: () => void;
  onOpenDetails: (profile: SetupsAnalyticsProfile) => void;
}) {
  return <Dialog open={open} onOpenChange={(next) => { if (!next) onClose(); }}>
    {open && <DialogContent className="max-w-xl" aria-label="Профили, выпустившие сетапы">
      <DialogHeader>
        <DialogTitle>Профили, выпустившие сетапы</DialogTitle>
        <DialogDescription>Список зависит от алгоритма и диапазона выпуска. Нажмите ID профиля для подробностей.</DialogDescription>
      </DialogHeader>
      <div className="flex flex-wrap gap-2">
        <Button variant="outline" onClick={() => { for (const profile of profiles) onSelectionChange(profile.slotId, true); }}>Выбрать все</Button>
        <Button variant="outline" onClick={() => { for (const profile of profiles) onSelectionChange(profile.slotId, false); }}>Снять все</Button>
      </div>
      <div className="max-h-96 overflow-y-auto">
        <Table>
          <TableHeader>
            <TableRow>
              <TableHead>Выбран</TableHead>
              <TableHead>Профиль</TableHead>
              <TableHead>Направление</TableHead>
              <TableHead numeric>Сетапов</TableHead>
            </TableRow>
          </TableHeader>
          <TableBody>
            {profiles.map((profile) => <TableRow key={profile.slotId}>
              <TableCell><Checkbox checked={selection.has(profile.slotId)} aria-label={`Профиль ${profile.slotId}`} onCheckedChange={(checked) => onSelectionChange(profile.slotId, checked === true)} /></TableCell>
              <TableCell><button type="button" className="font-medium text-primary underline-offset-4 hover:underline" onClick={() => onOpenDetails(profile)}>№{profile.slotId}</button></TableCell>
              <TableCell><Badge variant={profile.direction === "short" ? "danger" : "success"}>{profile.direction.toUpperCase()}</Badge></TableCell>
              <TableCell numeric>{profile.setupCount}</TableCell>
            </TableRow>)}
          </TableBody>
        </Table>
      </div>
      <div className="flex flex-wrap items-center justify-between gap-2">
        <p className="text-xs text-muted-foreground">Выбрано: {selection.size} из {profiles.length}</p>
        <div className="flex gap-2">
          <Button variant="outline" onClick={onClose}>Отмена</Button>
          <Button onClick={onApply}>Готово</Button>
        </div>
      </div>
    </DialogContent>}
  </Dialog>;
}

export function SetupsAnalyticsPanel() {
  const [algorithms, setAlgorithms] = useState<Algorithm[] | null>(null);
  const [algorithmId, setAlgorithmId] = useState("");
  const [fromInput, setFromInput] = useState(() => localDateInput(Math.floor(Date.now() / 1000) - 7 * 86400));
  const [toInput, setToInput] = useState(() => localDateInput(Math.floor(Date.now() / 1000)));
  const [profiles, setProfiles] = useState<SetupsAnalyticsProfile[]>([]);
  const [profilesState, setProfilesState] = useState<"idle" | "loading" | "ready" | "error">("idle");  const [profilesError, setProfilesError] = useState<string | null>(null);
  const [selectedSlots, setSelectedSlots] = useState<ReadonlySet<number> | null>(null);
  const [pickerOpen, setPickerOpen] = useState(false);
  const [pickerSelection, setPickerSelection] = useState<ReadonlySet<number>>(new Set());
  const [detailProfile, setDetailProfile] = useState<SetupsAnalyticsProfile | null>(null);
  const [result, setResult] = useState<SetupsAnalyticsResult | null>(null);
  const [running, setRunning] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const calculateRef = useRef<AbortController | null>(null);

  const range = useMemo(() => {
    try {
      return { value: replayRange(fromInput, toInput), error: null as string | null };
    } catch (cause) {
      return { value: null, error: errorText(cause) };
    }
  }, [fromInput, toInput]);
  const rangeFrom = range.value?.fromUnix ?? null;
  const rangeTo = range.value?.toUnix ?? null;
  const profilesStatus = algorithmId === "" || rangeFrom === null || rangeTo === null ? "idle" : profilesState;

  useEffect(() => {
    const controller = new AbortController();
    void fetchPlatform<{ algorithms: Algorithm[] }>("/v1/algorithms", controller.signal).then((state) => {
      if (controller.signal.aborted) return;
      const list = state.kind === "ready" && Array.isArray(state.data.algorithms) ? state.data.algorithms : [];
      setAlgorithms(list);
      setAlgorithmId((current) => current === "" && list[0] !== undefined ? list[0].id : current);
      if (state.kind === "unreachable") setProfilesError(`Список алгоритмов недоступен (${state.detail})`);
    });
    return () => controller.abort();
  }, []);

  useEffect(() => {
    if (algorithmId === "" || rangeFrom === null || rangeTo === null) return;
    const controller = new AbortController();
    const timer = window.setTimeout(() => {
      setProfilesState("loading");
      void fetchSetupsAnalyticsProfiles(algorithmId, rangeFrom, rangeTo, controller.signal)
        .then((items) => {
          if (controller.signal.aborted) return;
          setProfiles(items);
          setProfilesState("ready");
          setProfilesError(null);
          setSelectedSlots((current) => current === null ? null : new Set(items.filter((profile) => current.has(profile.slotId)).map((profile) => profile.slotId)));
        })
        .catch((cause) => {
          if (controller.signal.aborted) return;
          setProfiles([]);
          setProfilesState("error");
          setProfilesError(errorText(cause));
        });
    }, PROFILES_DEBOUNCE_MS);
    return () => { controller.abort(); window.clearTimeout(timer); };
  }, [algorithmId, rangeFrom, rangeTo]);

  useEffect(() => () => calculateRef.current?.abort(), []);

  function openPicker() {
    setPickerSelection(selectedSlots === null ? new Set(profiles.map((profile) => profile.slotId)) : new Set(selectedSlots));
    setPickerOpen(true);
  }

  function togglePickerSlot(slotId: number, checked: boolean) {
    setPickerSelection((current) => {
      const next = new Set(current);
      if (checked) next.add(slotId);
      else next.delete(slotId);
      return next;
    });
  }

  function openProfileDetails(slotId: number) {
    setDetailProfile(profiles.find((profile) => profile.slotId === slotId) ?? null);
  }

  async function run() {
    if (running || algorithmId === "" || range.value === null) return;
    calculateRef.current?.abort();
    const controller = new AbortController();
    calculateRef.current = controller;
    setRunning(true);
    setError(null);
    try {
      const slots = selectedSlots === null ? null : [...selectedSlots].sort((left, right) => left - right);
      const data = await fetchSetupsAnalytics(algorithmId, range.value.fromUnix, range.value.toUnix, slots, controller.signal);
      if (!controller.signal.aborted) setResult(data);
    } catch (cause) {
      if (!controller.signal.aborted) setError(errorText(cause));
    } finally {
      if (calculateRef.current === controller) {
        calculateRef.current = null;
        setRunning(false);
      }
    }
  }

  const profilesLabel = selectedSlots === null
    ? `Все (${profiles.length})`
    : `${selectedSlots.size} из ${profiles.length}`;

  return <section aria-label="Аналитика выпущенных сетапов" className="grid min-w-0 gap-4">
    <div className="flex items-center gap-2">
      <h2 className="text-lg font-semibold">Аналитика выпущенных сетапов</h2>
      <InfoNote>Разовый расчёт по сохранённым рекомендациям адаптивных профилей: вход по рекомендованному времени и гейту смещения цены, исход TP/SL по реальным минутным свечам за 7 суток. Результаты не кешируются.</InfoNote>
    </div>
    <Card>
      <CardContent className="flex flex-wrap items-end gap-3">
        <Field label="Алгоритм" className="w-full max-w-xs">
          {(control) => <Select {...control} value={algorithmId} disabled={algorithms === null} onChange={(event) => { setAlgorithmId(event.target.value); setResult(null); }}>
            {(algorithms ?? []).map((algorithm) => <option key={algorithm.id} value={algorithm.id}>{algorithmDisplayName(algorithm)}</option>)}
          </Select>}
        </Field>
        <Field label="Начало (момент выхода сетапа)" className="w-60">
          {(control) => <DateInput {...control} type="datetime-local" value={fromInput} onChange={(event) => { setFromInput(utcMidnightOnDateChange(fromInput, event.target.value)); setResult(null); }} />}
        </Field>
        <Field label="Конец" className="w-60">
          {(control) => <DateInput {...control} type="datetime-local" value={toInput} onChange={(event) => { setToInput(utcMidnightOnDateChange(toInput, event.target.value)); setResult(null); }} />}
        </Field>
        <Button variant="outline" disabled={profiles.length === 0} onClick={openPicker}>Профили: {profilesLabel}</Button>
        <Button disabled={running || algorithmId === "" || range.value === null} onClick={() => { void run(); }}>{running ? "Расчёт…" : "Рассчитать"}</Button>
        {profilesStatus === "loading" && <Spinner label="Загрузка профилей…" />}
        <p className="w-full text-xs text-muted-foreground">Время: {localTimezoneLabel()}. Смена даты сбрасывает время к 00:00 UTC. Расчёт выполняется по нажатию кнопки, без кеша.</p>
        {range.error !== null && <p role="alert" className="w-full text-xs text-danger">{range.error}</p>}
      </CardContent>
    </Card>
    {profilesError && <Alert variant="destructive" role="alert"><AlertDescription>{profilesError}</AlertDescription></Alert>}
    {error && <Alert variant="destructive" role="alert"><AlertDescription>{error}</AlertDescription></Alert>}
    {running && <Spinner label="Выполняется расчёт исходов сетапов…" />}
    {result && <Card>
      <CardContent className="grid gap-4">
        <div className="flex flex-wrap items-center gap-6">
          <div>
            <p className="text-sm text-muted-foreground">Среднее матожидание закрытых сделок</p>
            <p data-testid="setups-analytics-mean" className={`text-4xl font-semibold tabular-nums ${meanColorClass(result.totals.meanR)}`}>{formatRisk(result.totals.meanR)}</p>
            <p className="text-xs text-muted-foreground">закрытых сделок: {result.totals.closed}</p>
          </div>
          <div className="flex flex-col items-start gap-1">
            <span className="text-xs text-muted-foreground">Прошли гейт / выбрано</span>
            <GateBadge passed={result.totals.gatePassed} selected={result.totals.selected} />
          </div>
        </div>
        {result.studies.length > 0 && <div>
          <h3 className="mb-2 text-sm font-semibold text-muted-foreground">Исследования-основы</h3>
          <ul className="grid gap-2">
            {result.studies.map((study) => <li key={study.studyId} className="flex flex-wrap items-center gap-2 text-sm">
              <GateBadge passed={study.gatePassed} selected={study.selected} />
              <span>Исследование №{study.studyId}</span>
              <span className={`font-medium tabular-nums ${meanColorClass(study.meanR)}`}>{formatRisk(study.meanR)}</span>
              <span className="text-xs text-muted-foreground">закрытых: {study.closed}</span>
            </li>)}
          </ul>
        </div>}
        {result.setups.length === 0 ? <EmptyState title="Нет сетапов по выбранным фильтрам" description="Измените диапазон, алгоритм или набор профилей." /> : <div className="overflow-x-auto rounded-md border border-input/60">
          <Table>
            <caption className="px-3 py-2 text-left text-xs text-muted-foreground">Время: {localTimezoneLabel()}</caption>
            <TableHeader>
              <TableRow className="hover:bg-transparent">
                <TableHead>Инструмент</TableHead>
                <TableHead>Время сетапа</TableHead>
                <TableHead numeric>Вход</TableHead>
                <TableHead numeric className="text-success">TP</TableHead>
                <TableHead numeric className="text-danger">SL</TableHead>
                <TableHead>Закрытие</TableHead>
                <TableHead numeric>Результат</TableHead>
                <TableHead>Статус</TableHead>
                <TableHead numeric>Профиль</TableHead>
              </TableRow>
            </TableHeader>
            <TableBody>
              {result.setups.map((setup) => <SetupOutcomeRow key={setup.id} setup={setup} onOpenProfile={openProfileDetails} />)}
            </TableBody>
          </Table>
        </div>}
      </CardContent>
    </Card>}
    <ProfilePickerDialog
      open={pickerOpen}
      profiles={profiles}
      selection={pickerSelection}
      onSelectionChange={togglePickerSlot}
      onClose={() => setPickerOpen(false)}
      onApply={() => { setSelectedSlots(pickerSelection.size === profiles.length ? null : new Set(pickerSelection)); setPickerOpen(false); }}
      onOpenDetails={setDetailProfile}
    />
    <ProfileDetailsDialog profile={detailProfile} onClose={() => setDetailProfile(null)} />
  </section>;
}
