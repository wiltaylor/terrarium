// The run history of one schedule: `GET /api/schedules/{id}/runs`, newest
// first, a page at a time ("Load more"), and live runs from the `schedule`
// events. No JSX and no effects; the tests drive it with the mock API.

import { createSignal } from "solid-js";
import type { Api } from "../../api/client";
import type { ScheduleRun } from "../../api/types";
import { applyPage, emptyPaged, mergePage, type PagedState } from "../../lib/paging";

export const RUNS_PAGE = 10;

export function createRunHistory(api: Pick<Api, "schedules">, scheduleId: string, pageSize = RUNS_PAGE) {
  const [state, setState] = createSignal<PagedState<ScheduleRun>>(emptyPaged<ScheduleRun>());
  const [loading, setLoading] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  let generation = 0;
  const key = (r: ScheduleRun) => r.id;

  const load = async (reset: boolean) => {
    const gen = reset ? ++generation : generation;
    setLoading(true);
    try {
      const cursor = reset ? undefined : (state().next ?? undefined);
      const page = await api.schedules.runs(scheduleId, { limit: pageSize, ...(cursor ? { cursor } : {}) });
      if (gen !== generation) return;
      setState((s) => applyPage(s, page, key, reset));
      setError(null);
    } catch (e) {
      if (gen === generation) setError(e);
    } finally {
      if (gen === generation) setLoading(false);
    }
  };

  return {
    runs: () => state().items,
    hasMore: () => state().next !== null,
    initial: () => state().initial,
    loading,
    error,
    /** Read the first page (again). */
    reload: () => load(true),
    loadMore: () => (state().next ? load(false) : Promise.resolve()),
    /**
     * Read the first page again and merge it on top: new runs go first,
     * known ones are replaced, and the pages after it and the cursor stay.
     * The daemon's events carry only the run ID, so this follows them.
     */
    async syncHead() {
      if (state().initial) return load(true);
      const gen = generation;
      try {
        const page = await api.schedules.runs(scheduleId, { limit: pageSize });
        if (gen !== generation) return;
        setState((s) => {
          const fresh = new Map(page.items.map((r) => [r.id, r]));
          const known = s.items.map((r) => fresh.get(r.id) ?? r);
          const top = page.items.filter((r) => !s.items.some((x) => x.id === r.id));
          return { ...s, items: [...top, ...known] };
        });
        setError(null);
      } catch (e) {
        if (gen === generation) setError(e);
      }
    },
    /** A run from an event: a new one goes on top; a known one is replaced. */
    apply(run: ScheduleRun) {
      if (run.schedule && run.schedule !== scheduleId) return;
      setState((s) => {
        const known = s.items.some((x) => x.id === run.id);
        const items = known ? s.items.map((x) => (x.id === run.id ? run : x)) : mergePage([run], s.items, key);
        return { ...s, items };
      });
    },
  };
}

export type RunHistory = ReturnType<typeof createRunHistory>;
