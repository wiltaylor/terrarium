// Cursor pagination helpers (docs/api.md §2.4). Pure; no Solid imports.

import type { Page } from "../api/http";

export type { Page };

/** `limit` and `cursor`, the two paging parameters of every list endpoint. */
export interface PageQuery {
  limit?: number;
  cursor?: string;
}

export const DEFAULT_LIMIT = 100;
export const MAX_LIMIT = 1000;

/** Follow `next` until the last page. `maxPages` stops a runaway list. */
export async function collectAll<T>(fetchPage: (cursor?: string) => Promise<Page<T>>, maxPages = 50): Promise<T[]> {
  const out: T[] = [];
  let cursor: string | undefined;
  for (let i = 0; i < maxPages; i++) {
    const page = await fetchPage(cursor);
    out.push(...page.items);
    if (!page.next || page.next === cursor) return out;
    cursor = page.next;
  }
  return out;
}

/**
 * Cut one page out of a full, ordered list, the way the daemon does: the
 * cursor is the key of the last item of the previous page. The mock uses it.
 */
export function paginate<T>(items: T[], q: PageQuery = {}, key: (t: T) => string): Page<T> {
  const limit = Math.max(1, Math.min(q.limit ?? DEFAULT_LIMIT, MAX_LIMIT));
  let start = 0;
  if (q.cursor) {
    const i = items.findIndex((t) => key(t) === q.cursor);
    start = i < 0 ? items.length : i + 1;
  }
  const slice = items.slice(start, start + limit);
  const more = start + limit < items.length;
  return { items: slice, next: more && slice.length ? key(slice[slice.length - 1]!) : null };
}

/** Append a page to a list, dropping items whose key is already there. */
export function mergePage<T>(current: T[], page: T[], key: (t: T) => string): T[] {
  const seen = new Set(current.map(key));
  return [...current, ...page.filter((t) => !seen.has(key(t)))];
}

/** The state of a "load more" list, as a pure reducer. */
export interface PagedState<T> {
  items: T[];
  next: string | null;
  /** True before the first page arrives. */
  initial: boolean;
}

export const emptyPaged = <T>(): PagedState<T> => ({ items: [], next: null, initial: true });

export function applyPage<T>(state: PagedState<T>, page: Page<T>, key: (t: T) => string, reset = false): PagedState<T> {
  return {
    items: reset ? page.items : mergePage(state.items, page.items, key),
    next: page.next,
    initial: false,
  };
}
