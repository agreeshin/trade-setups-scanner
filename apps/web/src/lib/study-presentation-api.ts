import { apiFetch } from "./platform-api";

export interface StudyPresentationCounts { closed: number; tp: number; sl: number }

/** A successful publication, independent of the replacement actual job's state. */
export interface StudyPresentationSnapshot {
  revision: number;
  source: "initial" | "actual";
  sourceRevision: number | null;
  observationCutoff: number;
  publishedAt: string;
  meanR: number | null;
  counts: StudyPresentationCounts | null;
  selectedObjectiveR?: number | null;
  winRate?: number | null;
}
export type CurrentStudyPresentation = { status: "available"; snapshot: StudyPresentationSnapshot }
  | { status: "unavailable"; reason: string };

export class StudyPresentationApiError extends Error {
  constructor(message: string, readonly status?: number, readonly code?: string) { super(message); }
}
const record = (value: unknown): value is Record<string, unknown> => value !== null && typeof value === "object" && !Array.isArray(value);
const count = (value: unknown): value is number => typeof value === "number" && Number.isSafeInteger(value) && value >= 0;
const nullableNumber = (value: unknown): value is number | null => value === null || (typeof value === "number" && Number.isFinite(value));

export function parseCurrentStudyPresentation(value: unknown): CurrentStudyPresentation {
  const invalid = () => new StudyPresentationApiError("Некорректный сохранённый PNG-снимок исследования.");
  if (!record(value)) throw invalid();
  if (value.status === "unavailable" && typeof value.reason === "string" && value.reason.length > 0 && value.reason.length <= 2000) {
    return { status: "unavailable", reason: value.reason };
  }
  const snapshot = value.snapshot;
  if (value.status !== "available" || !record(snapshot) || !count(snapshot.revision) || snapshot.revision === 0
    || !["initial", "actual"].includes(String(snapshot.source))
    || (snapshot.source === "initial" ? snapshot.sourceRevision !== null : !count(snapshot.sourceRevision) || snapshot.sourceRevision === 0)
    || !count(snapshot.observationCutoff) || !Number.isFinite(new Date(snapshot.observationCutoff * 1000).getTime())
    || typeof snapshot.publishedAt !== "string" || snapshot.publishedAt.length > 128
    || !/(?:Z|[+-]\d{2}:\d{2})$/.test(snapshot.publishedAt) || !Number.isFinite(Date.parse(snapshot.publishedAt))
    || !nullableNumber(snapshot.meanR)
    || !(snapshot.selectedObjectiveR === undefined || nullableNumber(snapshot.selectedObjectiveR))
    || !(snapshot.winRate === undefined || (nullableNumber(snapshot.winRate) && (snapshot.winRate === null || (snapshot.winRate >= 0 && snapshot.winRate <= 100))))) throw invalid();
  let counts: StudyPresentationCounts | null = null;
  if (snapshot.counts !== null) {
    const raw = snapshot.counts;
    if (!record(raw) || !count(raw.closed) || !count(raw.tp) || !count(raw.sl)
      || raw.closed !== raw.tp + raw.sl || (raw.closed === 0 && snapshot.meanR !== null)) throw invalid();
    counts = { closed: raw.closed, tp: raw.tp, sl: raw.sl };
  }
  return { status: "available", snapshot: { revision: snapshot.revision, source: snapshot.source as StudyPresentationSnapshot["source"],
    sourceRevision: snapshot.sourceRevision as number | null, observationCutoff: snapshot.observationCutoff, publishedAt: snapshot.publishedAt,
    meanR: snapshot.meanR, counts,
    ...(snapshot.selectedObjectiveR === undefined ? {} : { selectedObjectiveR: snapshot.selectedObjectiveR }),
    ...(snapshot.winRate === undefined ? {} : { winRate: snapshot.winRate }) } };
}

/** Read persisted bytes only. The caller owns the Blob and any object URL it creates. */
export async function fetchStudyPresentationPng(studyId: number, revision: number, signal: AbortSignal): Promise<Blob> {
  const response = await apiFetch(`/v1/optimization-studies/${studyId}/presentation.png?revision=${revision}`, { signal });
  if (!response.ok) {
    const body: unknown = await response.json().catch(() => null);
    const code = record(body) && typeof body.error === "string" ? body.error : undefined;
    const message = response.status === 401 ? "Сессия истекла. Войдите заново."
      : code === "study_presentation_revision_changed" ? "PNG-снимок обновился. Загрузите текущую версию."
      : response.status === 404 ? "Сохранённый PNG исследования недоступен."
      : `PNG исследования недоступен: HTTP ${response.status}`;
    throw new StudyPresentationApiError(message, response.status, code);
  }
  const maxBytes = 256 * 1024;
  if (response.headers.get("content-type")?.split(";", 1)[0]?.trim().toLowerCase() !== "image/png"
    || Number(response.headers.get("content-length")) > maxBytes) throw new StudyPresentationApiError("Сервер вернул неподдерживаемый PNG исследования.");
  const blob = await response.blob();
  if (blob.size === 0 || blob.size > maxBytes) throw new StudyPresentationApiError("Превышен размер сохранённого PNG исследования или изображение пусто.");
  return blob;
}
