import { useRef, useState } from "react";
import { putOptimizationDraft } from "../lib/optimization-api";
import type { DomainDrafts, ScopeDraft, SearchField } from "./optimization-form";

interface SharedScopeDraft {
  symbols: string;
  direction: "short" | "long";
  horizonMinutes: string;
  tpLevels: string;
  slLevels: string;
  minClosedPerCell: string;
  seed: string;
  maxTrials: string;
  maxSeconds: string;
}

export interface OptimizationFormDraft {
  version: 1;
  shared: {
    scope: SharedScopeDraft;
    algorithmDomains: DomainDrafts;
    analyticsDomains: DomainDrafts;
  };
  manual: {
    from: string;
    to: string;
    setupMaturity: string;
    minSetupsPerDay: string;
  };
  continuous: {
    lookbackDays: string;
    minSetupsPerDay: string;
    minMeanExpectancy: string;
  };
}

type UnknownRecord = Record<string, unknown>;

function record(value: unknown): UnknownRecord | null {
  return typeof value === "object" && value !== null && !Array.isArray(value) ? value as UnknownRecord : null;
}

function text(value: unknown): string | null {
  return typeof value === "string" ? value : null;
}

function restoreDomainMap(fields: SearchField[], value: unknown, fallback: DomainDrafts): DomainDrafts {
  const source = record(value);
  return Object.fromEntries(fields.map((field) => {
    const stored = record(source?.[field.key]);
    const defaultValue = fallback[field.key]!;
    return [field.key, {
      baseline: text(stored?.baseline) ?? defaultValue.baseline,
      search: typeof stored?.search === "boolean" ? stored.search : defaultValue.search,
      values: text(stored?.values) ?? defaultValue.values,
    }];
  }));
}

export function restoreOptimizationDraft(
  value: unknown,
  algorithmFields: SearchField[],
  analyticsFields: SearchField[],
  fallbackScope: ScopeDraft,
  fallbackAlgorithmDomains: DomainDrafts,
  fallbackAnalyticsDomains: DomainDrafts,
): OptimizationFormDraft | null {
  const root = record(value);
  const shared = record(root?.shared);
  const storedScope = record(shared?.scope);
  const manual = record(root?.manual);
  const continuous = record(root?.continuous);
  if (root?.version !== 1 || !shared || !storedScope || !manual || !continuous) return null;
  const direction = storedScope.direction === "long" || storedScope.direction === "short" ? storedScope.direction : fallbackScope.direction;
  const sharedScope: SharedScopeDraft = {
    symbols: text(storedScope.symbols) ?? fallbackScope.symbols,
    direction,
    horizonMinutes: text(storedScope.horizonMinutes) ?? fallbackScope.horizonMinutes,
    tpLevels: text(storedScope.tpLevels) ?? fallbackScope.tpLevels,
    slLevels: text(storedScope.slLevels) ?? fallbackScope.slLevels,
    minClosedPerCell: text(storedScope.minClosedPerCell) ?? fallbackScope.minClosedPerCell,
    seed: text(storedScope.seed) ?? fallbackScope.seed,
    maxTrials: text(storedScope.maxTrials) ?? fallbackScope.maxTrials,
    maxSeconds: text(storedScope.maxSeconds) ?? fallbackScope.maxSeconds,
  };
  return {
    version: 1,
    shared: {
      scope: sharedScope,
      algorithmDomains: restoreDomainMap(algorithmFields, shared.algorithmDomains, fallbackAlgorithmDomains),
      analyticsDomains: restoreDomainMap(analyticsFields, shared.analyticsDomains, fallbackAnalyticsDomains),
    },
    manual: {
      from: text(manual.from) ?? fallbackScope.from,
      to: text(manual.to) ?? fallbackScope.to,
      setupMaturity: text(manual.setupMaturity) ?? fallbackScope.setupMaturity,
      minSetupsPerDay: text(manual.minSetupsPerDay) ?? fallbackScope.minSetupsPerDay,
    },
    continuous: {
      lookbackDays: text(continuous.lookbackDays) ?? "30",
      minSetupsPerDay: text(continuous.minSetupsPerDay) ?? "4",
      minMeanExpectancy: text(continuous.minMeanExpectancy) ?? "0.15",
    },
  };
}

function sharedScope(scope: ScopeDraft): SharedScopeDraft {
  return {
    symbols: scope.symbols,
    direction: scope.direction,
    horizonMinutes: scope.horizonMinutes,
    tpLevels: scope.tpLevels,
    slLevels: scope.slLevels,
    minClosedPerCell: scope.minClosedPerCell,
    seed: scope.seed,
    maxTrials: scope.maxTrials,
    maxSeconds: scope.maxSeconds,
  };
}

export function createOptimizationDraft(
  scope: ScopeDraft,
  algorithmDomains: DomainDrafts,
  analyticsDomains: DomainDrafts,
  previous: OptimizationFormDraft | null,
  mode: { manual: OptimizationFormDraft["manual"] } | { continuous: OptimizationFormDraft["continuous"] },
): OptimizationFormDraft {
  const fallbackManual = { from: scope.from, to: scope.to, setupMaturity: scope.setupMaturity, minSetupsPerDay: scope.minSetupsPerDay };
  const next: OptimizationFormDraft = {
    version: 1,
    shared: { scope: sharedScope(scope), algorithmDomains, analyticsDomains },
    manual: previous?.manual ?? fallbackManual,
    continuous: previous?.continuous ?? { lookbackDays: "30", minSetupsPerDay: "4", minMeanExpectancy: "0.15" },
  };
  return "manual" in mode ? { ...next, manual: mode.manual } : { ...next, continuous: mode.continuous };
}

export function applyOptimizationDraftScope(scope: ScopeDraft, draft: OptimizationFormDraft, mode: "manual" | "continuous"): ScopeDraft {
  return {
    ...scope,
    ...draft.shared.scope,
    ...(mode === "manual" ? draft.manual : { setupMaturity: "0" }),
  };
}

export function useOptimizationDraftSave(algorithmId: string) {
  const queue = useRef<Promise<void>>(Promise.resolve());
  const [saveError, setSaveError] = useState<string | null>(null);
  function save(value: OptimizationFormDraft): void {
    setSaveError(null);
    queue.current = queue.current.catch(() => undefined).then(async () => {
      await putOptimizationDraft(algorithmId, value);
    }).catch((cause: unknown) => {
      setSaveError(cause instanceof Error ? cause.message : "Не удалось сохранить настройки оптимизации.");
    });
  }
  return { save, saveError };
}
