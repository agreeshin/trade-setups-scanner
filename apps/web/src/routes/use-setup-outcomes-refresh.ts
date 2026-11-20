import { useCallback, useEffect, useRef, useState } from "react";
import { refreshSetupOutcomesPage } from "../lib/setup-outcomes-api";
import { errorText } from "./platform-controls";

interface RefreshState {
  pending: boolean;
  processed: number;
  computed: number;
  updated: number;
  completedAt: number | null;
  error: string | null;
  revision: number;
}

/** Owned by the keep-alive Setups page, across both subsections. No retained setup collections. */
export function useSetupOutcomesRefresh(active: boolean) {
  const attempted = useRef(false);
  const mounted = useRef(false);
  const operation = useRef<AbortController | null>(null);
  const [state, setState] = useState<RefreshState>({ pending: false, processed: 0, computed: 0, updated: 0, completedAt: null, error: null, revision: 0 });

  useEffect(() => {
    mounted.current = true;
    return () => {
      mounted.current = false;
      const current = operation.current;
      // StrictMode replays cleanup/setup synchronously. Abort only a real unmount, not that replay.
      queueMicrotask(() => { if (!mounted.current && operation.current === current) current?.abort(); });
    };
  }, []);

  const refresh = useCallback(async () => {
    if (operation.current !== null || !mounted.current) return;
    attempted.current = true;
    const controller = new AbortController();
    operation.current = controller;
    setState((previous) => ({ ...previous, pending: true, processed: 0, computed: 0, updated: 0, error: null }));
    let processed = 0;
    let computed = 0;
    let updated = 0;
    let cursor: string | undefined;
    let snapshotId: number | undefined;
    try {
      do {
        const page = await refreshSetupOutcomesPage(cursor, controller.signal);
        if (controller.signal.aborted || !mounted.current) return;
        if (snapshotId !== undefined && page.snapshotId !== snapshotId) throw new Error("Изменился снимок обновления. Повторите обновление вручную.");
        snapshotId = page.snapshotId;
        processed += page.processed; computed += page.computed; updated += page.updated;
        if (![processed, computed, updated].every(Number.isSafeInteger)) throw new Error("Превышен допустимый объём обновления результатов.");
        setState((previous) => ({ ...previous, processed, computed, updated }));
        cursor = page.nextCursor ?? undefined;
      } while (cursor !== undefined);
      setState((previous) => ({ ...previous, pending: false, completedAt: Date.now() / 1000, revision: previous.revision + (updated > 0 ? 1 : 0) }));
    } catch (cause) {
      if (!controller.signal.aborted && mounted.current) {
        setState((previous) => ({ ...previous, pending: false, error: errorText(cause).slice(0, 400), revision: previous.revision + (updated > 0 ? 1 : 0) }));
      }
    } finally {
      if (operation.current === controller) operation.current = null;
    }
  }, []);

  useEffect(() => {
    if (!active || attempted.current) return;
    attempted.current = true;
    void refresh();
  }, [active, refresh]);

  return { ...state, refresh };
}
