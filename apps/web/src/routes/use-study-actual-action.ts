import { useEffect, useRef, useState } from "react";
import { getStudyActualSummary, isStudyActualPending, refreshStudyActual, StudyActualApiError, type StudyActualSummary } from "../lib/study-actual-api";
import { errorText } from "./platform-controls";

const rank = { not_calculated: 0, queued: 1, running: 2, completed: 3, failed: 3 };

/** One row owns one mutation; compact discovery remains the history page's responsibility. */
export function useStudyActualAction(studyId: number, seed: StudyActualSummary | null | undefined, onChanged: () => void) {
  const [accepted, setAccepted] = useState<StudyActualSummary | null>(null);
  const [submitting, setSubmitting] = useState(false);
  const [uncertain, setUncertain] = useState(false);
  const [error, setError] = useState<string | null>(null);
  const request = useRef<AbortController | null>(null);
  const mounted = useRef(false);
  const actual = accepted === null || (seed && (seed.revision > accepted.revision || (seed.revision === accepted.revision && rank[seed.status] >= rank[accepted.status]))) ? seed : accepted;
  useEffect(() => {
    mounted.current = true;
    return () => { mounted.current = false; request.current?.abort(); };
  }, []);
  async function command(reconcileOnly = false) {
    if (request.current || (!reconcileOnly && (uncertain || !actual?.eligibility.eligible || isStudyActualPending(actual)))) return;
    const controller = new AbortController(); request.current = controller;
    setSubmitting(true); setError(null);
    const accept = (next: StudyActualSummary) => {
      if (!mounted.current || controller.signal.aborted) return;
      setAccepted(next); setUncertain(false); onChanged();
    };
    try {
      accept(reconcileOnly ? await getStudyActualSummary(studyId, controller.signal) : await refreshStudyActual(studyId, controller.signal));
    } catch (cause) {
      if (!mounted.current || controller.signal.aborted) return;
      if (cause instanceof StudyActualApiError && cause.code === "study_actual_busy" && cause.current !== null) {
        accept(cause.current);
      } else if (!reconcileOnly && (!(cause instanceof StudyActualApiError) || cause.status === undefined || cause.status >= 500 || cause.code === "study_actual_busy")) {
        // A lost POST acknowledgement is not permission to submit a second job.
        setUncertain(true);
        try { accept(await getStudyActualSummary(studyId, controller.signal)); }
        catch (readCause) {
          if (mounted.current && !controller.signal.aborted) setError(`Не удалось подтвердить статус расчёта: ${errorText(readCause)}. Новый запуск заблокирован до проверки статуса.`);
        }
      } else {
        setError(errorText(cause));
        if (!reconcileOnly) onChanged();
      }
    } finally {
      if (request.current === controller) request.current = null;
      if (mounted.current && !controller.signal.aborted) setSubmitting(false);
    }
  }
  return { actual, submitting, uncertain, error, pending: submitting || uncertain || isStudyActualPending(actual),
    refresh: () => command(), reconcile: () => command(true) };
}
