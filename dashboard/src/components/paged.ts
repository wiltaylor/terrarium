// A "load more" list over a cursor endpoint, for one view. The paging logic
// is the pure `applyPage` in lib/paging; this file only holds the signals.

import { createEffect, createSignal, on } from "solid-js";
import type { Page } from "../api/http";
import { applyPage, emptyPaged, mergePage, type PagedState } from "../lib/paging";

export interface PagedList<T> {
  items: () => T[];
  hasMore: () => boolean;
  loading: () => boolean;
  error: () => unknown;
  /** True until the first page arrives. */
  initial: () => boolean;
  loadMore: () => Promise<void>;
  /** Load the first page again. */
  reload: () => Promise<void>;
  /** Add or replace items without a request, for live updates. */
  upsert: (items: T[], where?: "start" | "end") => void;
}

/**
 * `fetchPage(key, cursor)` loads one page. `source` is the reactive key (for
 * example the project name); a change resets the list. `null` waits.
 */
export function createPagedList<T, K>(
  source: () => K | null | undefined,
  fetchPage: (key: K, cursor?: string) => Promise<Page<T>>,
  keyOf: (t: T) => string,
): PagedList<T> {
  const [state, setState] = createSignal<PagedState<T>>(emptyPaged<T>());
  const [loading, setLoading] = createSignal(false);
  const [error, setError] = createSignal<unknown>(null);
  let generation = 0;

  const load = async (reset: boolean) => {
    const k = source();
    if (k === null || k === undefined) return;
    const gen = reset ? ++generation : generation;
    setLoading(true);
    try {
      const page = await fetchPage(k, reset ? undefined : (state().next ?? undefined));
      if (gen !== generation) return;
      setState((s) => applyPage(s, page, keyOf, reset));
      setError(null);
    } catch (e) {
      if (gen === generation) setError(e);
    } finally {
      if (gen === generation) setLoading(false);
    }
  };

  createEffect(
    on(source, () => {
      setState(emptyPaged<T>());
      void load(true);
    }),
  );

  return {
    items: () => state().items,
    hasMore: () => state().next !== null,
    loading,
    error,
    initial: () => state().initial,
    loadMore: () => (state().next ? load(false) : Promise.resolve()),
    reload: () => load(true),
    upsert(items, where = "end") {
      setState((s) => {
        const replaced = s.items.map((x) => items.find((y) => keyOf(y) === keyOf(x)) ?? x);
        const fresh = items.filter((y) => !s.items.some((x) => keyOf(x) === keyOf(y)));
        return { ...s, items: where === "start" ? mergePage(fresh, replaced, keyOf) : [...replaced, ...fresh] };
      });
    },
  };
}
