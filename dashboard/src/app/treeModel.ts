// The sidebar project tree: which projects are open, and the filter. Pure, so tests can read it.

import { createSignal } from "solid-js";
import type { KeyValueStorage } from "../stores/auth";

export const EXPANDED_KEY = "terrarium.tree.expanded";

/** Above this many projects, the tree shows a filter input. */
export const FILTER_THRESHOLD = 8;

/** The project in a /p/<name>/... path, or null. */
export function projectFromPath(pathname: string): string | null {
  const m = /^\/p\/([^/]+)/.exec(pathname);
  if (!m) return null;
  try {
    return decodeURIComponent(m[1]!);
  } catch {
    return m[1]!;
  }
}

/** The stored list: an array of strings, or empty for anything else. */
export function parseExpanded(raw: string | null): string[] {
  if (!raw) return [];
  try {
    const v: unknown = JSON.parse(raw);
    return Array.isArray(v) ? v.filter((x): x is string => typeof x === "string") : [];
  } catch {
    return [];
  }
}

/** Add `name` to the open set (no duplicates). */
export function expand(list: readonly string[], name: string): string[] {
  return list.includes(name) ? [...list] : [...list, name];
}

/** Remove `name` from the open set. */
export function collapse(list: readonly string[], name: string): string[] {
  return list.filter((n) => n !== name);
}

/** Open `name` if it is closed, close it if it is open. */
export function toggle(list: readonly string[], name: string): string[] {
  return list.includes(name) ? collapse(list, name) : expand(list, name);
}

/** Drop the names of projects that are gone. An empty `known` list (not loaded yet) keeps all. */
export function prune(list: readonly string[], known: readonly string[]): string[] {
  if (known.length === 0) return [...list];
  return list.filter((n) => known.includes(n));
}

/** The projects whose name holds the query (case-insensitive). An empty query keeps all. */
export function filterProjects<T extends { name: string }>(projects: readonly T[], query: string): T[] {
  const q = query.trim().toLowerCase();
  if (!q) return [...projects];
  return projects.filter((p) => p.name.toLowerCase().includes(q));
}

/**
 * The open set, persisted. `follow(path)` opens the project of a new URL, so
 * the project you go to is always open; you can close it again after that.
 */
export function createExpandedStore(storage: KeyValueStorage | null = null) {
  const read = () => {
    try {
      return parseExpanded(storage?.getItem(EXPANDED_KEY) ?? null);
    } catch {
      return [];
    }
  };
  const [list, setList] = createSignal<string[]>(read());
  const write = (next: string[]) => {
    setList(next);
    try {
      storage?.setItem(EXPANDED_KEY, JSON.stringify(next));
    } catch {
      // ignore
    }
  };
  let lastFollowed: string | null = null;

  return {
    list,
    isOpen: (name: string) => list().includes(name),
    toggle: (name: string) => write(toggle(list(), name)),
    open: (name: string) => {
      if (!list().includes(name)) write(expand(list(), name));
    },
    close: (name: string) => {
      if (list().includes(name)) write(collapse(list(), name));
    },
    /** Open the project of `pathname` when the URL moves to a different project. */
    follow: (pathname: string) => {
      const p = projectFromPath(pathname);
      if (p === lastFollowed) return;
      lastFollowed = p;
      if (p && !list().includes(p)) write(expand(list(), p));
    },
    prune: (known: readonly string[]) => {
      const next = prune(list(), known);
      if (next.length !== list().length) write(next);
    },
  };
}

export type ExpandedStore = ReturnType<typeof createExpandedStore>;
